//! The editor's side of a collaborative session: which files are shared,
//! which buffers hold them, and how they are kept in sync.
//!
//! The actor in the parent module only moves messages between topics. The
//! rules of the protocol live here, in one place, so that the actor's
//! topics, the service's files and each buffer's replica stay in agreement.

use std::{cell::Cell, collections::HashMap};

use anyhow::{ensure, Result};
use helix_core::{
    crdt::{replica_id, RemoteOperation, Replica, SharedId},
    history::History,
    Selection, Transaction,
};
use helix_event::register_hook;
use tokio::sync::mpsc::UnboundedSender;

use super::{
    proto::{Announcement, FileMessage},
    Event, Request, Transport,
};
use crate::{
    editor::Action,
    events::{DocumentDidChange, DocumentDidClose},
    Document, DocumentId, Editor, ViewId,
};

/// The editor's side of the session, next to the handle to its transport.
///
/// The files are kept here and not in the actor because the editor reads
/// them on its own thread, like the picker listing them, and couldn't wait
/// on the actor for it.
#[derive(Default)]
pub struct Service {
    pub transport: Transport,
    /// Every file announced in the current session, open or not.
    pub files: HashMap<SharedId, Announcement>,
    /// Files we subscribed to and are waiting on a snapshot of, with the
    /// empty buffer that will hold each.
    pub pending: HashMap<SharedId, DocumentId>,
}

impl Service {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn send(&self, request: Request) {
        self.transport.send(request);
    }
}

/// Share the focused buffer with the session.
pub fn share(editor: &mut Editor) -> Result<()> {
    let owner = editor.p2p_service.transport.id;
    let doc = doc_mut!(editor);
    ensure!(doc.crdt.is_none(), "buffer is already shared");

    // Peers see the path relative to the workspace,
    // or in full when the file is outside of it.
    let path = doc.path().map(|path| {
        let (workspace, _) = helix_loader::find_workspace();
        path.strip_prefix(&workspace).unwrap_or(path).to_path_buf()
    });

    let replica = Replica::new(replica_id(), owner, path, doc.text());
    let announcement = Announcement {
        id: replica.shared_id(),
        owner,
        path: replica.path().map(ToOwned::to_owned),
    };
    doc.crdt = Some(replica);

    // Join the file's topic before announcing it, so whoever opens
    // the file finds us there.
    let service = &mut editor.p2p_service;
    service.send(Request::Subscribe(announcement.clone()));
    service.send(Request::Announce(announcement.clone()));
    service.files.insert(announcement.id, announcement);
    Ok(())
}

/// The buffer that holds a shared file, if we have it open or are waiting
/// on its contents.
pub fn buffer(editor: &Editor, id: SharedId) -> Option<DocumentId> {
    let open = editor
        .documents()
        .find(|doc| doc.shared_id() == Some(id))
        .map(Document::id);
    // A buffer still waiting on its contents counts too, unless it was
    // closed in the meantime.
    let pending = editor
        .p2p_service
        .pending
        .get(&id)
        .copied()
        .filter(|doc| editor.documents.contains_key(doc));
    open.or(pending)
}

/// Open a shared file, joining its topic unless we have it already.
pub fn open(editor: &mut Editor, announcement: &Announcement, action: Action) {
    if let Some(doc) = buffer(editor, announcement.id) {
        editor.switch(doc, action);
        return;
    }

    // Open the buffer right away, while the view the file was picked for is
    // still the one in focus. It stays empty until someone in the file's
    // topic sends us its contents.
    let doc = editor.new_file(action);
    editor.p2p_service.pending.insert(announcement.id, doc);
    // Subscribing twice is harmless, the actor ignores it.
    editor
        .p2p_service
        .send(Request::Subscribe(announcement.clone()));
    editor.set_status(format!("opening {}", announcement.id.fmt_short()));
}

/// Leave the session.
pub fn close(editor: &mut Editor) {
    stop_sharing(editor);
    editor.p2p_service.send(Request::Close);
}

pub fn handle_event(editor: &mut Editor, event: Event) {
    match event {
        Event::Connected(peer) => {
            editor.set_status(format!("connected with {}", peer.fmt_short()));

            // Announcements go out once, so a peer that joins later would
            // never learn about the files shared before it. Everyone
            // announces every file it knows of instead, which is cheap since
            // announcements carry no contents.
            let service = &editor.p2p_service;
            for announcement in service.files.values() {
                service.send(Request::Announce(announcement.clone()));
            }
        }
        Event::Announced(announcement) => announced(editor, announcement),
        Event::FileConnected(id) => send_snapshot(editor, id),
        Event::File(id, FileMessage::Snapshot { text, replica }) => {
            receive_snapshot(editor, id, text, &replica)
        }
        Event::File(id, FileMessage::Edit(op)) => receive_edit(editor, id, &op),
        // Our edits to the file no longer reach anyone, so stop treating it
        // as shared rather than let it silently drift apart.
        Event::FileLeft(id) => {
            editor.p2p_service.pending.remove(&id);
            if let Some(doc) = editor
                .documents_mut()
                .find(|doc| doc.shared_id() == Some(id))
            {
                doc.crdt = None;
            }
        }
        // Same as leaving with close, but the session ended on its own.
        Event::Left => stop_sharing(editor),
        Event::Error(err) => editor.set_error(err),
    }
}

/// Send local edits to the others, and leave a file's topic once its
/// buffer is closed.
pub fn register_hooks(requests: UnboundedSender<Request>) {
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        // Edits that came from the others are theirs to send.
        if event.ghost_transaction || event.remote_transaction {
            return Ok(());
        }
        let Some(replica) = event.doc.crdt.as_mut() else {
            return Ok(());
        };

        let id = replica.shared_id();
        for op in replica.from_local(event.changes) {
            let _ = requests.send(Request::Broadcast(id, FileMessage::Edit(op)));
        }

        Ok(())
    });

    // Once the buffer is gone there is nothing to apply the file's edits to,
    // so leave its topic. Opening the file again rejoins it.
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        if let Some(id) = event.doc.shared_id() {
            event.editor.p2p_service.send(Request::Unsubscribe(id));
        }
        Ok(())
    });
}

fn announced(editor: &mut Editor, announcement: Announcement) {
    // Files are announced again whenever someone connects.
    if editor.p2p_service.files.contains_key(&announcement.id) {
        return;
    }

    let owner = announcement.owner.fmt_short();
    let status = match &announcement.path {
        Some(path) => format!("{owner} shared {}", path.display()),
        None => format!("{owner} shared a buffer ({})", announcement.id.fmt_short()),
    };
    editor
        .p2p_service
        .files
        .insert(announcement.id, announcement);
    editor.set_status(status);
}

/// Someone joined the file's topic, so they opened the file and wait for its
/// contents. Gossip can't send to a single peer, so this reaches everyone in
/// the topic, and everyone who has the file answers. Receivers only take the
/// first snapshot.
fn send_snapshot(editor: &mut Editor, id: SharedId) {
    let Some(doc) = editor.documents().find(|doc| doc.shared_id() == Some(id)) else {
        return;
    };
    let Some(replica) = &doc.crdt else {
        return;
    };

    let message = FileMessage::Snapshot {
        text: doc.text().to_string(),
        replica: replica.encode(),
    };
    editor.p2p_service.send(Request::Broadcast(id, message));
}

fn receive_snapshot(editor: &mut Editor, id: SharedId, text: String, replica: &[u8]) {
    // Snapshots reach everyone in the topic, so only take the first one for
    // a file we asked for.
    let Some(doc_id) = editor.p2p_service.pending.remove(&id) else {
        return;
    };
    let Some(announcement) = editor.p2p_service.files.get(&id) else {
        return;
    };

    let crdt = match Replica::decode(
        id,
        announcement.owner,
        announcement.path.clone(),
        replica_id(),
        replica,
    ) {
        Ok(crdt) => crdt,
        Err(err) => {
            editor.set_error(format!("failed to open shared buffer: {err:#}"));
            return;
        }
    };

    let view_id = view_of(editor, doc_id);
    let Some(doc) = editor.documents.get_mut(&doc_id) else {
        // The buffer was closed while waiting. It had no replica yet, so
        // closing it didn't leave the file's topic.
        editor.p2p_service.send(Request::Unsubscribe(id));
        return;
    };

    // Replace whatever the buffer holds, as it only ever held a placeholder.
    // The replica is attached afterwards, so this change isn't sent to the
    // others as an edit of our own.
    doc.ensure_view_init(view_id);
    let transaction = Transaction::change(
        doc.text(),
        [(0, doc.text().len_chars(), Some(text.into()))].into_iter(),
    );
    doc.apply(&transaction, view_id);
    doc.set_selection(view_id, Selection::point(0));

    // Start the history from the snapshot. Otherwise the next undo would
    // take the snapshot with it, emptying the buffer, and send that to
    // everyone as a deletion of the whole file.
    doc.append_changes_to_history(editor.tree.get_mut(view_id));
    doc.history = Cell::new(History::default());

    doc.crdt = Some(crdt);
}

fn receive_edit(editor: &mut Editor, id: SharedId, op: &RemoteOperation) {
    let Some(doc_id) = buffer(editor, id) else {
        return;
    };
    let view_id = view_of(editor, doc_id);
    let doc = doc_mut!(editor, &doc_id);

    // apply reads the document's selection for view_id, which a buffer that
    // view has never displayed does not have yet.
    doc.ensure_view_init(view_id);

    // Taken out of the document, as from_remote reads the document's text
    // while it changes the replica. A buffer still waiting on its contents
    // has no replica yet, and the edit is lost.
    let Some(mut crdt) = doc.crdt.take() else {
        return;
    };
    if let Some(transaction) = crdt.from_remote(doc.text(), op) {
        doc.apply(&transaction, view_id);
    }
    doc.crdt = Some(crdt);
}

/// Stop sharing every buffer and forget the session's files, as they were
/// announced in a session we are no longer in.
fn stop_sharing(editor: &mut Editor) {
    for doc in editor.documents_mut() {
        doc.crdt = None;
    }
    editor.p2p_service.files.clear();
    editor.p2p_service.pending.clear();
}

/// A view that shows the buffer, or the focused one when none does. Changes
/// are applied for a view, which maps its selection through them.
fn view_of(editor: &Editor, doc_id: DocumentId) -> ViewId {
    editor
        .tree
        .views()
        .find(|(view, _)| view.doc == doc_id)
        .map_or(editor.tree.focus, |(view, _)| view.id)
}
