use helix_event::register_hook;
use helix_view::{
    events::DocumentDidChange,
    p2p::{self, proto::FileMessage},
};
use tokio::sync::mpsc::UnboundedSender;

pub fn register_hooks(requests: UnboundedSender<p2p::Request>) {
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event.ghost_transaction || event.remote_transaction {
            return Ok(());
        }
        let Some(replica) = event.doc.crdt.as_mut() else {
            return Ok(());
        };

        let id = replica.shared_id();
        for op in replica.from_local(event.changes) {
            let _ = requests.send(p2p::Request::Broadcast(id, FileMessage::Edit(op)));
        }

        Ok(())
    });
}
