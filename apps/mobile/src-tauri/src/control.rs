use std::collections::{BTreeMap, BTreeSet, VecDeque};

const LIST_FMT: &str = "#{window_id} #{pane_id} #{pane_active} #{window_active} #{window_name}";
const MAX_HISTORY: u32 = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Output { pane: String, data: String },
    Begin,
    Reply { ok: bool, body: Vec<String> },
    TopologyChanged,
    SessionChanged,
    Exit,
}

#[derive(Default)]
struct Parser {
    buffer: String,
    reply: Option<Vec<String>>,
}

impl Parser {
    fn write(&mut self, chunk: &str) -> Vec<Event> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();
        while let Some(index) = self.buffer.find('\n') {
            let mut line = self.buffer[..index].to_string();
            self.buffer.drain(..=index);
            if line.ends_with('\r') {
                line.pop();
            }
            self.parse_line(&strip_markers(&line), &mut events);
        }
        events
    }

    fn parse_line(&mut self, line: &str, events: &mut Vec<Event>) {
        if let Some(reply) = self.reply.as_mut() {
            if line == "%end"
                || line.starts_with("%end ")
                || line == "%error"
                || line.starts_with("%error ")
            {
                let ok = line.starts_with("%end");
                events.push(Event::Reply {
                    ok,
                    body: self.reply.take().unwrap_or_default(),
                });
            } else {
                reply.push(line.to_string());
            }
            return;
        }
        if !line.starts_with('%') {
            return;
        }
        let (keyword, rest) = line
            .split_once(' ')
            .map_or((line, ""), |(keyword, rest)| (keyword, rest));
        match keyword {
            "%output" => {
                if let Some((pane, data)) = rest.split_once(' ') {
                    if is_pane_id(pane) {
                        events.push(Event::Output {
                            pane: pane.into(),
                            data: unescape_output(data),
                        });
                    }
                }
            }
            "%begin" => {
                self.reply = Some(Vec::new());
                events.push(Event::Begin);
            }
            "%end" | "%error" => events.push(Event::Reply {
                ok: keyword == "%end",
                body: Vec::new(),
            }),
            "%session-changed" => events.push(Event::SessionChanged),
            "%window-add"
            | "%window-close"
            | "%unlinked-window-close"
            | "%window-renamed"
            | "%window-pane-changed"
            | "%session-window-changed"
            | "%layout-change"
            | "%sessions-changed" => events.push(Event::TopologyChanged),
            "%exit" => events.push(Event::Exit),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneRow {
    window: String,
    pane: String,
    pane_active: bool,
    window_active: bool,
    name: String,
}

#[derive(Default)]
struct WindowRegistry {
    order: Vec<String>,
    names: BTreeMap<String, String>,
    pane_to_window: BTreeMap<String, String>,
    active: Option<String>,
}

struct RegistryDiff {
    added: Vec<String>,
    removed: Vec<String>,
    renamed: Vec<(String, String)>,
    active: Option<String>,
    active_changed: bool,
}

impl WindowRegistry {
    fn reconcile(&mut self, rows: &[PaneRow]) -> RegistryDiff {
        let previous_order = self.order.clone();
        let previous_names = self.names.clone();
        let previous_active = self.active.clone();
        let mut order = Vec::new();
        let mut names = BTreeMap::new();
        let mut pane_to_window = BTreeMap::new();
        let mut active = None;
        for row in rows {
            if !order.contains(&row.window) {
                order.push(row.window.clone());
            }
            names.insert(
                row.window.clone(),
                if row.name.is_empty() {
                    row.window.clone()
                } else {
                    row.name.clone()
                },
            );
            pane_to_window.insert(row.pane.clone(), row.window.clone());
            if row.window_active || (row.pane_active && active.is_none()) {
                active = Some(row.window.clone());
            }
        }
        if active.is_none() {
            active = previous_active
                .clone()
                .filter(|window| names.contains_key(window))
                .or_else(|| order.first().cloned());
        }
        let before: BTreeSet<_> = previous_order.iter().cloned().collect();
        let after: BTreeSet<_> = order.iter().cloned().collect();
        let added = order
            .iter()
            .filter(|window| !before.contains(*window))
            .cloned()
            .collect();
        let removed = previous_order
            .iter()
            .filter(|window| !after.contains(*window))
            .cloned()
            .collect();
        let renamed = names
            .iter()
            .filter_map(|(window, name)| {
                previous_names
                    .get(window)
                    .filter(|previous| *previous != name)
                    .map(|_| (window.clone(), name.clone()))
            })
            .collect();
        let active_changed = active != previous_active;
        self.order = order;
        self.names = names;
        self.pane_to_window = pane_to_window;
        self.active = active.clone();
        RegistryDiff {
            added,
            removed,
            renamed,
            active,
            active_changed,
        }
    }
}

#[derive(Debug, Clone)]
enum ReplyKind {
    Initial,
    Ignore,
    Topology,
    Capture(String),
    Cursor { window: String, body: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Send(String),
    Data { window: String, data: String },
    WindowAdd { window: String, order: Vec<String> },
    WindowClose { window: String, order: Vec<String> },
    WindowRename { window: String, name: String },
    WindowActive { window: String, order: Vec<String> },
    Ready,
    Exit,
}

pub struct ControlEngine {
    parser: Parser,
    registry: WindowRegistry,
    replies: VecDeque<ReplyKind>,
    utf8_carry: BTreeMap<String, Vec<u8>>,
    pending_output: BTreeMap<String, String>,
    pending_input: Vec<(String, Option<String>)>,
    pending_captures: BTreeSet<String>,
    capture_input: Vec<(String, String)>,
    ready: bool,
    session: String,
}

impl ControlEngine {
    pub fn new(session: String) -> Self {
        let mut replies = VecDeque::new();
        // tmux replies once to the command used to enter control mode. Treat that reply specially
        // so we actively request topology instead of depending on a best-effort session event.
        replies.push_back(ReplyKind::Initial);
        Self {
            parser: Parser::default(),
            registry: WindowRegistry::default(),
            replies,
            utf8_carry: BTreeMap::new(),
            pending_output: BTreeMap::new(),
            pending_input: Vec::new(),
            pending_captures: BTreeSet::new(),
            capture_input: Vec::new(),
            ready: false,
            session,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let latin1: String = bytes.iter().map(|byte| *byte as char).collect();
        let mut actions = Vec::new();
        for event in self.parser.write(&latin1) {
            match event {
                Event::Output { pane, data } => {
                    if !self.route_output(pane, data, &mut actions) {
                        self.queue_topology(&mut actions);
                    }
                }
                Event::SessionChanged | Event::TopologyChanged => self.queue_topology(&mut actions),
                Event::Reply { ok, body } => self.handle_reply(ok, body, &mut actions),
                Event::Exit => actions.push(Action::Exit),
                Event::Begin => {}
            }
        }
        actions
    }

    pub fn input(&mut self, data: String, target: Option<String>) -> Vec<Action> {
        if !self.ready {
            self.pending_input.push((data, target));
            return Vec::new();
        }
        self.input_actions(&data, target.as_deref())
    }

    pub fn resize(&mut self, cols: u32, rows: u32) -> Vec<Action> {
        self.send(
            format!("refresh-client -C {}x{}", cols.max(1), rows.max(1)),
            ReplyKind::Ignore,
        )
    }

    pub fn new_window(&mut self) -> Vec<Action> {
        self.send(
            format!(
                "new-window -t {} -c \"#{{pane_current_path}}\"",
                self.session
            ),
            ReplyKind::Ignore,
        )
    }

    pub fn select_window(&mut self, window: &str) -> Vec<Action> {
        if !is_window_id(window) {
            return Vec::new();
        }
        self.send(format!("select-window -t {window}"), ReplyKind::Ignore)
    }

    pub fn close_window(&mut self, window: &str) -> Vec<Action> {
        if !is_window_id(window) {
            return Vec::new();
        }
        self.send(format!("kill-window -t {window}"), ReplyKind::Ignore)
    }

    pub fn rename_window(&mut self, window: &str, title: &str) -> Vec<Action> {
        if !is_window_id(window) {
            return Vec::new();
        }
        let title = sanitize_title(title);
        let command = if title.is_empty() {
            format!("set-window-option -t {window} automatic-rename on")
        } else {
            format!("rename-window -t {window} \"{title}\"")
        };
        self.send(command, ReplyKind::Ignore)
    }

    pub fn capture_window(&mut self, window: &str) -> Vec<Action> {
        if !is_window_id(window) || !self.pending_captures.insert(window.into()) {
            return Vec::new();
        }
        self.send(
            format!("capture-pane -p -e -q -S -{MAX_HISTORY} -t {window}"),
            ReplyKind::Capture(window.into()),
        )
    }

    fn input_actions(&mut self, data: &str, target: Option<&str>) -> Vec<Action> {
        let target = target
            .filter(|target| is_window_id(target))
            .map(str::to_string)
            .or_else(|| self.registry.active.clone());
        let Some(target) = target else {
            return Vec::new();
        };
        if self.pending_captures.contains(&target) {
            self.capture_input.push((data.into(), target));
            return Vec::new();
        }
        let mut actions = Vec::new();
        for command in encode_send_keys(data, &target) {
            actions.extend(self.send(command, ReplyKind::Ignore));
        }
        actions
    }

    fn queue_topology(&mut self, actions: &mut Vec<Action>) {
        if self
            .replies
            .iter()
            .any(|reply| matches!(reply, ReplyKind::Topology))
        {
            return;
        }
        actions.extend(self.send(
            format!("list-panes -s -F \"{LIST_FMT}\" -t {}", self.session),
            ReplyKind::Topology,
        ));
    }

    fn send(&mut self, command: String, reply: ReplyKind) -> Vec<Action> {
        self.replies.push_back(reply);
        vec![Action::Send(command)]
    }

    fn handle_reply(&mut self, ok: bool, body: Vec<String>, actions: &mut Vec<Action>) {
        let Some(reply) = self.replies.pop_front() else {
            return;
        };
        if !ok {
            match reply {
                ReplyKind::Capture(window) | ReplyKind::Cursor { window, .. } => {
                    self.finish_capture(&window, actions);
                }
                _ => {}
            }
            return;
        }
        match reply {
            ReplyKind::Initial => self.queue_topology(actions),
            ReplyKind::Ignore => {}
            ReplyKind::Topology => self.apply_topology(body, actions),
            ReplyKind::Capture(window) => {
                actions.extend(self.send(
                    format!("display-message -p -t {window} '#{{cursor_x}} #{{cursor_y}}'"),
                    ReplyKind::Cursor { window, body },
                ));
            }
            ReplyKind::Cursor {
                window,
                body: capture,
            } => {
                let cursor = body
                    .first()
                    .and_then(|line| line.split_once(' '))
                    .and_then(|(x, y)| Some((x.parse::<usize>().ok()?, y.parse::<usize>().ok()?)))
                    .unwrap_or((0, capture.len().saturating_sub(1)));
                let mut data = String::from("\x1b[2J\x1b[H");
                // Parser deliberately stores the SSH byte stream one byte per Rust char so tmux's
                // ASCII control protocol can be split safely. Capture replies are terminal bytes
                // too; convert that Latin-1-shaped storage back to bytes before UTF-8 decoding.
                // Sending the intermediate string directly produced one visible `â` for every
                // Unicode box-drawing character when an old session was restored.
                data.push_str(&decode_latin1_utf8(&capture.join("\r\n")));
                data.push_str(&format!("\x1b[{};{}H", cursor.1 + 1, cursor.0 + 1));
                actions.push(Action::Data {
                    window: window.clone(),
                    data,
                });
                self.finish_capture(&window, actions);
            }
        }
    }

    fn apply_topology(&mut self, body: Vec<String>, actions: &mut Vec<Action>) {
        let rows: Vec<_> = body.iter().filter_map(|line| parse_row(line)).collect();
        let diff = self.registry.reconcile(&rows);
        let pending = std::mem::take(&mut self.pending_output);
        for (pane, data) in pending {
            self.route_output(pane, data, actions);
        }
        for window in &diff.removed {
            actions.push(Action::WindowClose {
                window: window.clone(),
                order: self.registry.order.clone(),
            });
        }
        for window in &diff.added {
            actions.push(Action::WindowAdd {
                window: window.clone(),
                order: self.registry.order.clone(),
            });
            if let Some(name) = self.registry.names.get(window) {
                actions.push(Action::WindowRename {
                    window: window.clone(),
                    name: name.clone(),
                });
            }
        }
        for (window, name) in diff.renamed {
            actions.push(Action::WindowRename { window, name });
        }
        if diff.active_changed {
            if let Some(window) = diff.active {
                actions.push(Action::WindowActive {
                    window,
                    order: self.registry.order.clone(),
                });
            }
        }
        for window in diff.added {
            actions.extend(self.capture_window(&window));
        }
        if !self.ready && !self.registry.order.is_empty() {
            self.ready = true;
            actions.push(Action::Ready);
            let pending = std::mem::take(&mut self.pending_input);
            for (data, target) in pending {
                actions.extend(self.input_actions(&data, target.as_deref()));
            }
        }
    }

    fn route_output(&mut self, pane: String, data: String, actions: &mut Vec<Action>) -> bool {
        let Some(window) = self.registry.pane_to_window.get(&pane).cloned() else {
            self.pending_output.entry(pane).or_default().push_str(&data);
            return false;
        };
        let carry = self.utf8_carry.entry(pane).or_default();
        carry.extend(data.chars().map(|character| character as u8));
        let (text, tail) = decode_utf8_prefix(carry);
        *carry = tail;
        if !text.is_empty() {
            actions.push(Action::Data { window, data: text });
        }
        true
    }

    fn finish_capture(&mut self, window: &str, actions: &mut Vec<Action>) {
        self.pending_captures.remove(window);
        let mut deferred = Vec::new();
        let mut remaining = Vec::new();
        for (data, target) in std::mem::take(&mut self.capture_input) {
            if target == window {
                deferred.push((data, target));
            } else {
                remaining.push((data, target));
            }
        }
        self.capture_input = remaining;
        for (data, target) in deferred {
            actions.extend(self.input_actions(&data, Some(&target)));
        }
    }
}

fn strip_markers(line: &str) -> String {
    let line = line
        .strip_prefix("\x1bP1000p")
        .or_else(|| line.strip_prefix("P1000p"))
        .unwrap_or(line);
    line.strip_suffix("\x1b\\").unwrap_or(line).to_string()
}

fn unescape_output(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            if bytes[index + 1] == b'\\' {
                output.push('\\');
                index += 2;
                continue;
            }
            if index + 3 < bytes.len()
                && bytes[index + 1..=index + 3]
                    .iter()
                    .all(|byte| (b'0'..=b'7').contains(byte))
            {
                let code = (bytes[index + 1] - b'0') * 64
                    + (bytes[index + 2] - b'0') * 8
                    + (bytes[index + 3] - b'0');
                output.push(code as char);
                index += 4;
                continue;
            }
        }
        output.push(bytes[index] as char);
        index += 1;
    }
    output
}

fn parse_row(line: &str) -> Option<PaneRow> {
    let mut fields = line.splitn(5, ' ');
    let window = fields.next()?.to_string();
    let pane = fields.next()?.to_string();
    if !is_window_id(&window) || !is_pane_id(&pane) {
        return None;
    }
    Some(PaneRow {
        window,
        pane,
        pane_active: fields.next()? == "1",
        window_active: fields.next()? == "1",
        // list-panes is read through Parser's byte-preserving Latin-1 representation as well.
        // Decode user/program supplied tmux window titles before crossing the JSON boundary.
        name: decode_latin1_utf8(fields.next().unwrap_or_default()),
    })
}

fn is_window_id(value: &str) -> bool {
    value.strip_prefix('@').is_some_and(|tail| {
        !tail.is_empty() && tail.chars().all(|character| character.is_ascii_digit())
    })
}

fn is_pane_id(value: &str) -> bool {
    value.strip_prefix('%').is_some_and(|tail| {
        !tail.is_empty() && tail.chars().all(|character| character.is_ascii_digit())
    })
}

fn sanitize_title(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() && !matches!(character, '"' | '\\' | ';'))
        .take(80)
        .collect::<String>()
        .trim()
        .to_string()
}

fn encode_send_keys(data: &str, target: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut literal = String::new();
    let flush = |literal: &mut String, commands: &mut Vec<String>| {
        if literal.is_empty() {
            return;
        }
        let escaped = literal
            .chars()
            .map(|character| match character {
                '\\' => "\\\\".into(),
                '"' => "\\\"".into(),
                '\t' => "\\t".into(),
                '\x1b' => "\\e".into(),
                character if (character as u32) < 0x20 => format!("\\{:03o}", character as u32),
                character => character.to_string(),
            })
            .collect::<String>();
        commands.push(format!("send-keys -t {target} -l \"{escaped}\""));
        literal.clear();
    };
    let mut characters = data.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' || character == '\n' {
            flush(&mut literal, &mut commands);
            if character == '\r' && characters.peek() == Some(&'\n') {
                characters.next();
            }
            commands.push(format!("send-keys -t {target} Enter"));
        } else {
            literal.push(character);
        }
    }
    flush(&mut literal, &mut commands);
    commands
}

fn latin1_bytes(value: &str) -> Vec<u8> {
    value.chars().map(|character| character as u8).collect()
}

fn decode_latin1_utf8(value: &str) -> String {
    String::from_utf8_lossy(&latin1_bytes(value)).into_owned()
}

pub(crate) fn decode_utf8_prefix(bytes: &[u8]) -> (String, Vec<u8>) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_string(), Vec::new()),
        Err(error) => {
            let valid = error.valid_up_to();
            if error.error_len().is_none() {
                (
                    String::from_utf8_lossy(&bytes[..valid]).into_owned(),
                    bytes[valid..].to_vec(),
                )
            } else {
                (String::from_utf8_lossy(bytes).into_owned(), Vec::new())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_and_engine_build_native_window_actions() {
        let mut engine = ControlEngine::new("dt-one".into());
        let actions = engine.feed(b"%begin 1 1 0\r\n%end 1 1 0\r\n%session-changed $0 dt-one\r\n");
        assert!(actions.iter().any(
            |action| matches!(action, Action::Send(command) if command.starts_with("list-panes"))
        ));
        let actions = engine.feed(b"%begin 2 2 0\r\n@0 %0 1 1 shell\r\n%end 2 2 0\r\n");
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, Action::WindowAdd { window, .. } if window == "@0"))
        );
        assert!(actions.contains(&Action::Ready));
        assert!(actions.iter().any(
            |action| matches!(action, Action::Send(command) if command.starts_with("capture-pane"))
        ));
    }

    #[test]
    fn pane_output_reassembles_split_utf8() {
        let mut engine = ControlEngine::new("dt-one".into());
        engine
            .registry
            .pane_to_window
            .insert("%0".into(), "@0".into());
        assert!(engine.feed(b"%output %0 \\342\r\n").is_empty());
        let actions = engine.feed(b"%output %0 \\224\\200\r\n");
        assert!(actions.contains(&Action::Data {
            window: "@0".into(),
            data: "─".into(),
        }));
    }

    #[test]
    fn capture_and_window_names_decode_utf8_before_emitting() {
        let visible = "╭─中文🙂";
        let latin1 = visible
            .as_bytes()
            .iter()
            .map(|byte| *byte as char)
            .collect::<String>();
        let row = parse_row(&format!("@0 %0 1 1 {latin1}")).unwrap();
        assert_eq!(row.name, visible);

        let mut engine = ControlEngine::new("dt-one".into());
        engine.replies.clear();
        engine.replies.push_back(ReplyKind::Cursor {
            window: "@0".into(),
            body: vec![latin1],
        });
        let mut actions = Vec::new();
        engine.handle_reply(true, vec!["0 0".into()], &mut actions);
        assert!(actions.iter().any(|action| {
            matches!(action, Action::Data { data, .. } if data.contains(visible))
        }));
    }

    #[test]
    fn input_is_addressed_and_shell_metacharacters_are_escaped() {
        assert_eq!(
            encode_send_keys("say \"hi\"\n", "@2"),
            vec![
                "send-keys -t @2 -l \"say \\\"hi\\\"\"",
                "send-keys -t @2 Enter"
            ]
        );
        assert!(sanitize_title("bad;\"title\n").contains("badtitle"));
    }

    #[test]
    fn output_waits_for_topology_and_capture_gates_input() {
        let mut engine = ControlEngine::new("dt-one".into());
        engine.feed(b"%begin 1 1 0\r\n%end 1 1 0\r\n");
        let before = engine.feed(b"%output %0 hello\r\n");
        assert!(
            !before
                .iter()
                .any(|action| matches!(action, Action::Data { .. }))
        );
        let after = engine.feed(b"%begin 2 2 0\r\n@0 %0 1 1 shell\r\n%end 2 2 0\r\n");
        assert!(after.contains(&Action::Data {
            window: "@0".into(),
            data: "hello".into(),
        }));

        engine.ready = true;
        engine.pending_captures.insert("@0".into());
        assert!(engine.input("typed".into(), Some("@0".into())).is_empty());
        let mut released = Vec::new();
        engine.finish_capture("@0", &mut released);
        assert!(
            released
                .iter()
                .any(|action| matches!(action, Action::Send(command) if command.contains("typed")))
        );
    }
}
