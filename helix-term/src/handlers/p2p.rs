use helix_event::register_hook;
use helix_view::{
    events::DocumentDidChange,
    p2p::{
        net::Service,
        wire::{self, Message},
    },
};

pub fn register_hooks(p2p: Service) {
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event.ghost_transaction || event.remote_transaction {
            return Ok(());
        }
        let Some(shared) = event.doc.shared.as_mut() else {
            return Ok(());
        };

        let id = shared.id;
        for op in shared.replica.from_local(event.changes) {
            p2p.broadcast(wire::encode(&Message::Edit { id, op }));
        }

        Ok(())
    });
}
