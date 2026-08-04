use crate::{
    compositor::{Compositor, Context},
    key,
    runtime::{LayerCommand, RuntimeTaskEvent, UiCommand},
    ui::{
        overlay::overlaid,
        picker::{DynamicQuerySchedule, Injector, PickerKeyHandlers, PickerRefreshScope},
        Confirmation, Picker, PickerColumn,
    },
};
use helix_collab::{ParticipantId, Role};
use helix_view::Editor;

#[derive(Clone)]
struct ParticipantItem {
    id: ParticipantId,
    name: String,
    role: Role,
    identity: String,
    is_self: bool,
}

struct ParticipantPickerData {
    session: helix_collab::GuestSessionHandle,
    owner: bool,
}

#[derive(Clone, Copy)]
struct RoleItem(Role);

fn participant_items(editor: &Editor, local: ParticipantId) -> Vec<ParticipantItem> {
    let mut participants = editor
        .collaboration_participants()
        .map(|participant| ParticipantItem {
            id: participant.id,
            name: participant.name.clone(),
            role: participant.access,
            identity: if participant.id == local {
                String::from("you")
            } else {
                participant.id.to_string()[..8].to_owned()
            },
            is_self: participant.id == local,
        })
        .collect::<Vec<_>>();
    participants.sort_unstable_by(|left, right| {
        right
            .is_self
            .cmp(&left.is_self)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.id.cmp(&right.id))
    });
    participants
}

fn refresh_participants(
    _query: &str,
    editor: &mut Editor,
    data: std::sync::Arc<ParticipantPickerData>,
    injector: &Injector<ParticipantItem, ParticipantPickerData>,
    work: helix_runtime::Work,
    _block: helix_runtime::Block,
) -> helix_runtime::Task<anyhow::Result<()>> {
    let participants = participant_items(editor, data.session.participant().id);
    let injector = injector.clone();
    work.spawn(async move {
        for participant in participants {
            if injector.push(participant).is_err() {
                break;
            }
        }
        Ok(())
    })
}

pub(crate) fn push_participants(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
) {
    let Some(session) = editor.collaboration.session() else {
        editor.notify_warning("No collaboration session is active");
        return;
    };
    let local = session.participant().id;
    let owner = editor.collaboration.role() == Some(Role::Owner);
    let participants = participant_items(editor, local);

    let columns = [
        PickerColumn::new("name", |item: &ParticipantItem, _| {
            item.name.as_str().into()
        }),
        PickerColumn::new("role", |item: &ParticipantItem, _| {
            role_label(item.role).into()
        }),
        PickerColumn::new("identity", |item: &ParticipantItem, _| {
            item.identity.as_str().into()
        }),
    ];
    let data = ParticipantPickerData { session, owner };
    let project = data.session.project().id;
    let mut handlers: PickerKeyHandlers<ParticipantItem, ParticipantPickerData> =
        PickerKeyHandlers::new();
    handlers.insert(
        key!('r'),
        Box::new(|cx, item: &ParticipantItem, data, _| {
            if !data.owner {
                cx.editor
                    .notify_warning("Only the project owner can change participant roles");
                return;
            }
            if item.role == Role::Owner {
                cx.editor.notify_warning("The owner role cannot be changed");
                return;
            }
            cx.submit_ui(UiCommand::Layer(LayerCommand::CollaborationRolePicker {
                participant: item.id,
            }));
        }),
    );
    handlers.insert(
        key!('i'),
        Box::new(|cx, _item: &ParticipantItem, data, _| {
            if data.owner {
                cx.submit_ui(UiCommand::Layer(
                    LayerCommand::CollaborationInviteRolePicker,
                ));
            } else {
                cx.editor
                    .notify_warning("Only the project owner can create invitations");
            }
        }),
    );
    handlers.insert_confirmed(
        key!('d'),
        Box::new(|cx, item: &ParticipantItem, data, _| remove_confirmation(cx, item, &data)),
    );

    let picker = Picker::new(
        columns,
        0,
        participants,
        data,
        crate::ui::PickerRuntime::new(editor),
        ingress,
        |cx, item, _action| follow(cx, item),
    )
    .with_key_handlers(handlers)
    .with_dynamic_query(refresh_participants, DynamicQuerySchedule::Immediate)
    .with_refresh_scope(PickerRefreshScope::CollaborationParticipants(project))
    .show_preview(false);
    compositor.push(Box::new(overlaid(picker)));
}

pub(crate) fn push_role_picker(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    participant: ParticipantId,
) {
    let Some(session) = owner_session(editor) else {
        return;
    };
    let Some(target) = editor.participant(participant) else {
        editor.notify_warning("Participant is no longer connected");
        return;
    };
    if target.access == Role::Owner {
        editor.notify_warning("The owner role cannot be changed");
        return;
    }
    let name = target.name.clone();
    let columns = [PickerColumn::new("role", |item: &RoleItem, _| {
        role_label(item.0).into()
    })];
    let picker = Picker::new(
        columns,
        0,
        assignable_roles(),
        session,
        crate::ui::PickerRuntime::new(editor),
        ingress,
        move |cx, role, _action| {
            let session = cx.editor.collaboration.session();
            let name = name.clone();
            let role = role.0;
            let Some(session) = session else {
                cx.editor
                    .notify_warning("Collaboration session is no longer active");
                return;
            };
            cx.spawn_task_event(async move {
                session.set_role(participant, role).await?;
                Ok(RuntimeTaskEvent::CollaborationNotice(format!(
                    "Set {name}'s role to {}",
                    role_label(role)
                )))
            });
        },
    )
    .show_preview(false);
    compositor.push(Box::new(overlaid(picker)));
}

pub(crate) fn push_invite_role_picker(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
) {
    let Some(session) = owner_session(editor) else {
        return;
    };
    let columns = [PickerColumn::new("access", |item: &RoleItem, _| {
        role_label(item.0).into()
    })];
    let picker = Picker::new(
        columns,
        0,
        assignable_roles(),
        session,
        crate::ui::PickerRuntime::new(editor),
        ingress,
        |cx, role, _action| {
            let session = cx.editor.collaboration.session();
            let role = role.0;
            let Some(session) = session else {
                cx.editor
                    .notify_warning("Collaboration session is no longer active");
                return;
            };
            cx.spawn_task_event(async move {
                let code = session
                    .invite(
                        role,
                        crate::runtime::collaboration::now_unix_secs().saturating_add(60 * 60),
                    )
                    .await?;
                Ok(RuntimeTaskEvent::CollaborationInvitation(code))
            });
        },
    )
    .show_preview(false);
    compositor.push(Box::new(overlaid(picker)));
}

pub(crate) fn push_remove_confirmation(
    editor: &mut Editor,
    compositor: &mut Compositor,
    participant: ParticipantId,
) {
    let Some(session) = owner_session(editor) else {
        return;
    };
    let Some(item) = editor
        .participant(participant)
        .map(|participant| ParticipantItem {
            id: participant.id,
            name: participant.name.clone(),
            role: participant.access,
            identity: String::new(),
            is_self: participant.id == session.participant().id,
        })
    else {
        editor.notify_warning("Participant is no longer connected");
        return;
    };
    let data = ParticipantPickerData {
        session,
        owner: true,
    };
    let mut context = RemoveContext { editor };
    let Some(confirmation) = context.confirmation(&item, &data) else {
        return;
    };
    compositor.push(Box::new(confirmation.into_prompt()));
}

fn follow(cx: &mut Context, item: &ParticipantItem) {
    if item.is_self {
        cx.editor.collaboration.set_following(None);
        cx.editor.notify_info("Stopped following participant");
        return;
    }
    let Some(session) = cx.editor.collaboration.session() else {
        cx.editor
            .notify_warning("Collaboration session is no longer active");
        return;
    };
    let participant = item.id;
    let name = item.name.clone();
    cx.spawn_task_event(async move {
        session.follow(participant).await?;
        Ok(RuntimeTaskEvent::CollaborationNotice(format!(
            "Following {name}"
        )))
    });
}

fn remove_confirmation(
    cx: &mut Context,
    item: &ParticipantItem,
    data: &ParticipantPickerData,
) -> Option<Confirmation> {
    RemoveContext { editor: cx.editor }.confirmation(item, data)
}

struct RemoveContext<'a> {
    editor: &'a mut Editor,
}

impl RemoveContext<'_> {
    fn confirmation(
        &mut self,
        item: &ParticipantItem,
        data: &ParticipantPickerData,
    ) -> Option<Confirmation> {
        if !data.owner {
            self.editor
                .notify_warning("Only the project owner can remove participants");
            return None;
        }
        if item.is_self || item.role == Role::Owner {
            self.editor
                .notify_warning("The project owner cannot be removed");
            return None;
        }
        let participant = item.id;
        let name = item.name.clone();
        let session = data.session.clone();
        Some(Confirmation::new(
            format!("Remove {name} from this project?"),
            move |cx| {
                cx.spawn_task_event(async move {
                    session.remove_participant(participant).await?;
                    Ok(RuntimeTaskEvent::CollaborationNotice(format!(
                        "Removed {name}"
                    )))
                });
            },
        ))
    }
}

fn owner_session(editor: &mut Editor) -> Option<helix_collab::GuestSessionHandle> {
    let Some(session) = editor.collaboration.session() else {
        editor.notify_warning("No collaboration session is active");
        return None;
    };
    if editor.collaboration.role() != Some(Role::Owner) {
        editor.notify_warning("Only the project owner can manage access");
        return None;
    }
    Some(session)
}

fn assignable_roles() -> [RoleItem; 3] {
    [
        RoleItem(Role::Write),
        RoleItem(Role::Read),
        RoleItem(Role::Observe),
    ]
}

const fn role_label(role: Role) -> &'static str {
    match role {
        Role::Observe => "observe",
        Role::Read => "read",
        Role::Write => "write",
        Role::Owner => "owner",
    }
}
