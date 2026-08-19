use std::collections::HashMap;
use std::sync::Mutex;

pub const SCHEME: &str = "buoyhtml";

const PREVIEW_CSP: &str = "default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval' https:; style-src 'unsafe-inline' https:; font-src https: data:; img-src https: data: blob:; media-src https: data: blob:; connect-src https:; frame-src 'none'; form-action 'none'; base-uri 'none'";

#[derive(Default)]
pub struct PreviewStore {
    documents: Mutex<HashMap<String, Vec<u8>>>,
}

impl PreviewStore {
    pub fn put(&self, bytes: Vec<u8>) -> String {
        let token = token();
        if let Ok(mut documents) = self.documents.lock() {
            documents.clear();
            documents.insert(token.clone(), bytes);
        }
        token
    }

    pub fn response(&self, path: &str) -> (u16, &'static str, Vec<u8>) {
        let token = path
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .rsplit('/')
            .next()
            .unwrap_or_default();
        match self
            .documents
            .lock()
            .ok()
            .and_then(|documents| documents.get(token).cloned())
        {
            Some(document) => (200, PREVIEW_CSP, document),
            None => (
                404,
                "default-src 'none'",
                b"<!doctype html><title>404</title>not found".to_vec(),
            ),
        }
    }
}

fn token() -> String {
    use std::io::Read;
    let mut bytes = [0_u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        bytes = nanos.to_be_bytes();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_are_single_document_and_origin_isolated() {
        let store = PreviewStore::default();
        let first = store.put(b"first".to_vec());
        let second = store.put(b"second".to_vec());
        assert_eq!(store.response(&format!("/{first}")).0, 404);
        let (status, csp, body) = store.response(&format!("/{second}"));
        assert_eq!(status, 200);
        assert_eq!(body, b"second");
        assert!(csp.contains("script-src 'unsafe-inline'"));
        assert!(!csp.contains("'self'"));
    }
}
