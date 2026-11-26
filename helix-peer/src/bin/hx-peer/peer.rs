use std::{str::FromStr, sync::Arc};

use anyhow::Result;
use arc_swap::{access::Map, ArcSwap};
use futures_util::StreamExt;
use helix_loader::workspace_trust::WorkspaceTrust;
use helix_p2p::{crdt::Replica, net::TopicId, wire::SharedId};
use helix_term::{
    compositor::{self, Compositor},
    config::Config,
    job::Jobs,
    keymap::Keymaps,
    ui::EditorView,
};
use helix_view::{
    doc,
    editor::{Action, EditorEvent},
    graphics::Rect,
    input::{parse_macro, Event},
    Editor,
};
use serde_json::{json, Value};
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};

use helix_peer::{Buffer, Request, Response, State};

pub struct Peer {
    editor: Editor,
    compositor: Compositor,
    jobs: Jobs,
}

impl Peer {
    pub fn new(name: Option<String>) -> Self {
        let mut config = Config::default();
        config.editor.p2p.name = name;
        let trust = WorkspaceTrust::new((&config.editor.workspace_trust).into());
        let area = Rect::new(0, 0, 120, 40);

        let config = Arc::new(ArcSwap::from_pointee(config));
        let handlers = helix_term::setup_handlers(config.clone());
        let mut editor = Editor::new(
            area,
            Arc::new(helix_view::theme::Loader::new(&[])),
            Arc::new(ArcSwap::from_pointee(
                helix_core::config::default_lang_loader(),
            )),
            Arc::new(Map::new(config.clone(), |config: &Config| &config.editor)),
            handlers,
            trust,
        );
        let p2p = editor.p2p.as_ref().expect("p2p is enabled by default");
        helix_view::p2p::register_hooks(p2p);
        editor.new_file(Action::VerticalSplit);

        let mut compositor = Compositor::new(area);
        let keys = Box::new(Map::new(config, |config: &Config| &config.keys));
        compositor.push(Box::new(EditorView::new(Keymaps::new(keys))));

        Self {
            editor,
            compositor,
            jobs: Jobs::new(),
        }
    }

    /// Mirrors the application's event loop.
    pub async fn run(mut self) -> Result<()> {
        let mut lines = BufReader::new(stdin()).lines();
        let mut stdout = stdout();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Some(line) = line? else {
                        return Ok(());
                    };
                    let response = match self.handle(&line).await {
                        Ok(value) => Response::Ok(value),
                        Err(err) => Response::Error(format!("{err:#}")),
                    };
                    let mut response = serde_json::to_vec(&response)?;
                    response.push(b'\n');
                    stdout.write_all(&response).await?;
                    stdout.flush().await?;
                }
                Some(callback) = self.jobs.callbacks.recv() => {
                    self.callback(Ok(Some(callback)));
                }
                Some(callback) = self.jobs.wait_futures.next() => self.callback(callback),
                Some(message) = self.jobs.status_messages.recv() => {
                    self.editor.set_status(message.message);
                }
                event = self.editor.wait_event() => self.editor_event(event),
            }
            self.editor.sync_focus();
        }
    }

    fn callback(&mut self, callback: Result<Option<helix_term::job::Callback>>) {
        let job = self
            .jobs
            .handle_callback(&mut self.editor, &mut self.compositor, callback);
        if let Some(job) = job {
            self.jobs.add(job);
        }
    }

    fn editor_event(&mut self, event: EditorEvent) {
        match event {
            EditorEvent::P2pEvent(event) => self.editor.handle_p2p_event(event),
            EditorEvent::IdleTimer => {
                self.editor.clear_idle_timer();
                self.event(&Event::IdleTimeout);
            }
            _ => {}
        }
    }

    async fn handle(&mut self, line: &str) -> Result<Value> {
        let value = match serde_json::from_str(line)? {
            Request::Host => json!(self.editor.session_ticket()?.await),
            Request::Open { id } => {
                let id = SharedId::from(TopicId::from_str(&id)?);
                self.editor.open_shared_document(id, Action::Replace)?;
                Value::Null
            }
            Request::Keys { keys } => {
                for key in parse_macro(&keys)? {
                    self.event(&Event::Key(key));
                    // The application renders after every key, which syncs focus.
                    self.editor.sync_focus();
                }
                Value::Null
            }
            Request::State => json!(self.state()),
        };
        Ok(value)
    }

    fn event(&mut self, event: &Event) {
        let mut cx = compositor::Context {
            editor: &mut self.editor,
            jobs: &mut self.jobs,
            scroll: None,
        };
        self.compositor.handle_event(event, &mut cx);
    }

    fn state(&mut self) -> State {
        self.editor.drain_session_queue();
        let session = self.editor.session().ok();
        let buffers = self.editor.documents().filter_map(|doc| {
            let id = doc.shared?;
            let replica = session.and_then(|session| session.synced_replica(id));
            Some(Buffer {
                id: id.topic().to_string(),
                text: doc.text().to_string(),
                replica: replica.map(Replica::text),
                ops: replica.map_or(0, Replica::len_ops),
            })
        });
        State {
            focus: doc!(self.editor).shared.map(|id| id.topic().to_string()),
            buffers: buffers.collect(),
        }
    }
}
