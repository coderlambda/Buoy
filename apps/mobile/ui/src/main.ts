// The mobile package owns its app shell while sharing Buoy's transport-neutral session controller
// and terminal engine. Keeping this entry point here prevents future desktop layout work from
// changing the iPhone document or stylesheet.
import '../../../../ui/src/tauri-api.js';
import '../../../../ui/src/renderer.js';
