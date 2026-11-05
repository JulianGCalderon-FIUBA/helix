use anyhow::Context as _;
use helix_p2p::session::SharedMeta;
use helix_view::{doc, Editor};

use crate::{
    compositor::Compositor,
    job::{Callback, Jobs},
    ui::{overlay::overlaid, Picker, PickerColumn},
};

use super::{Context, PathStyleConfig};

pub fn session_new(cx: &mut Context) {
    run(cx, new_session);
}

pub fn session_join(cx: &mut Context) {
    run(cx, |editor, jobs| join_session(editor, jobs, None));
}

pub fn session_share(cx: &mut Context) {
    run(cx, |editor, _| share(editor));
}

pub fn session_unshare(cx: &mut Context) {
    run(cx, |editor, _| unshare(editor));
}

pub fn session_info(cx: &mut Context) {
    run(cx, |editor, _| info(editor));
}

pub fn session_close(cx: &mut Context) {
    run(cx, |editor, _| close(editor));
}

/// Opens a picker of every buffer shared in the session.
pub fn session_file_picker(cx: &mut Context) {
    // Each document with its owner's name.
    let items: Vec<(SharedMeta, String)> = match cx.editor.session() {
        Ok(session) => session
            .documents()
            .values()
            .map(|meta| (meta.clone(), session.name(meta.owner)))
            .collect(),
        Err(err) => return cx.editor.set_error(err.to_string()),
    };

    let columns = [
        PickerColumn::new("owner", |(_, owner): &(SharedMeta, String), _| {
            owner.as_str().into()
        }),
        PickerColumn::new(
            "path",
            |(meta, _): &(SharedMeta, String), config: &PathStyleConfig| {
                config.stylize(meta.path.as_deref(), None)
            },
        ),
        PickerColumn::new("id", |(meta, _): &(SharedMeta, String), _| {
            meta.id.fmt_short().to_string().into()
        }),
    ];

    let picker = Picker::new(
        columns,
        1,
        items,
        PathStyleConfig::new(&cx.editor.theme),
        |cx, (meta, _), action| {
            if let Err(err) = cx.editor.open_shared_document(meta.id, action) {
                cx.editor.set_error(err.to_string());
            }
        },
    )
    .with_preview(|editor, (meta, _)| {
        // Only buffers we subscribed to have preview.
        let doc = editor.shared_document(meta.id)?;
        let lines = doc.selections().values().next().map(|selection| {
            let cursor_line = selection.primary().cursor_line(doc.text().slice(..));
            (cursor_line, cursor_line)
        });
        Some((doc.id().into(), lines))
    });
    cx.push_layer(Box::new(overlaid(picker)));
}

/// Opens a picker of the other peers in the session.
pub fn session_peer_picker(cx: &mut Context) {
    // Each peer's name with how many documents they share.
    let items: Vec<(String, usize)> = match cx.editor.session() {
        Ok(session) => session
            .peers()
            .iter()
            .map(|(&peer, name)| {
                let documents = session.documents().values();
                let shared = documents.filter(|meta| meta.owner == peer).count();
                (name.clone(), shared)
            })
            .collect(),
        Err(err) => return cx.editor.set_error(err.to_string()),
    };

    let columns = [
        PickerColumn::new("peer", |(name, _): &(String, usize), _| {
            name.as_str().into()
        }),
        PickerColumn::new("shared", |(_, shared): &(String, usize), _| {
            shared.to_string().into()
        }),
    ];

    // TODO: jump to the peer when picked (#51).
    let picker = Picker::new(columns, 0, items, (), |_, _, _| {});
    cx.push_layer(Box::new(overlaid(picker)));
}

pub(super) fn new_session(editor: &mut Editor, jobs: &mut Jobs) -> anyhow::Result<()> {
    let ticket = editor.session_ticket()?;
    jobs.callback(async move {
        let ticket = ticket.await;
        Ok(Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, _: &mut Compositor| {
                let register = '+';
                match editor.registers.write(register, vec![ticket]) {
                    Ok(_) => editor.set_status(format!("yanked ticket to register {register}")),
                    Err(err) => editor.set_error(err.to_string()),
                }
            },
        )))
    });
    Ok(())
}

/// Without a ticket, takes the one in the clipboard.
pub(super) fn join_session(
    editor: &mut Editor,
    jobs: &mut Jobs,
    ticket: Option<&str>,
) -> anyhow::Result<()> {
    let ticket = match ticket {
        Some(ticket) => ticket.to_string(),
        None => editor
            .registers
            .first('+', editor)
            .context("no ticket in the clipboard")?
            .into_owned(),
    };

    let joined = editor.join_session(ticket.trim())?;
    editor.set_status("joining session");
    jobs.callback(async move {
        let result = joined.await;
        Ok(Callback::Editor(Box::new(
            move |editor: &mut Editor| match result {
                Ok(()) => editor.set_status("joined session"),
                Err(err) => editor.set_error(format!("failed to join session: {err:#}")),
            },
        )))
    });
    Ok(())
}

pub(super) fn share(editor: &mut Editor) -> anyhow::Result<()> {
    let doc_id = doc!(editor).id();
    let id = editor.share_document(doc_id)?;

    let label = editor.session()?.documents()[&id].label();
    editor.set_status(format!("shared {label}"));
    Ok(())
}

pub(super) fn unshare(editor: &mut Editor) -> anyhow::Result<()> {
    let doc = doc!(editor);
    let (doc_id, name) = (doc.id(), doc.display_name().into_owned());
    editor.unshare_document(doc_id)?;
    editor.set_status(format!("stopped syncing {name}"));
    Ok(())
}

pub(super) fn info(editor: &mut Editor) -> anyhow::Result<()> {
    let session = editor.session()?;
    let open = editor
        .documents()
        .filter(|doc| doc.shared.is_some())
        .count();
    let status = format!(
        "session {} · {} peers · {} shared, {open} open",
        session.topic().fmt_short(),
        session.peers().len(),
        session.documents().len(),
    );
    editor.set_status(status);
    Ok(())
}

pub(super) fn close(editor: &mut Editor) -> anyhow::Result<()> {
    editor.leave_session()?;
    editor.set_status("closed session");
    Ok(())
}

fn run(cx: &mut Context, f: impl FnOnce(&mut Editor, &mut Jobs) -> anyhow::Result<()>) {
    if let Err(err) = f(cx.editor, cx.jobs) {
        cx.editor.set_error(err.to_string());
    }
}
