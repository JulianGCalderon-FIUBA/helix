use helix_event::register_hook;
use helix_view::{
    events::DocumentDidChange,
    p2p::{self, proto::Message},
};
use tokio::sync::mpsc::UnboundedSender;

pub fn register_hooks(requests: UnboundedSender<p2p::Request>) {
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event.ghost_transaction || event.remote_transaction {
            return Ok(());
        }
        let Some(shared) = event.doc.shared.as_mut() else {
            return Ok(());
        };

        for op in shared.replica.from_local(event.changes) {
            let _ = requests.send(p2p::Request::Broadcast(Message::Edit { id: shared.id, op }));
        }

        Ok(())
    });
}
