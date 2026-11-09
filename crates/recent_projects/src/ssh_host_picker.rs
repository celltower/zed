//! Flat SSH host picker. Selecting a host opens the last project for that
//! SSH identity, or the remote home directory.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

use gpui::{
    AppContext, DismissEvent, Entity, EventEmitter, Focusable, Subscription, Task, WeakEntity,
};
use picker::Picker;
use project::Fs;
use remote::{
    RemoteConnectionIdentity, RemoteConnectionOptions, SshConnectionOptions,
    remote_client::ConnectionIdentifier,
};
use settings::{SshConnection, Settings as _};
use ui::{
    App, Context, HighlightedLabel, Icon, IconName, InteractiveElement, IntoElement, ListItem,
    ParentElement, Render, Styled, StyledExt, Toggleable, Window, div, h_flex, prelude::*,
};
use util::ResultExt as _;
use util::paths::PathStyle;
use workspace::{
    ModalView, MultiWorkspace, OpenMode, OpenOptions, RecentWorkspace, SerializedWorkspaceLocation,
    Workspace, WorkspaceDb, notifications::DetachAndPromptErr,
    open_remote_project_with_existing_connection,
};

use crate::{
    remote_connections::{RemoteConnectionModal, RemoteSettings, connect, open_remote_project},
    ssh_config::parse_ssh_config_host_entries,
};

#[derive(Clone)]
struct SshHost {
    display_name: SharedString,
    host_label: SharedString,
    connection: SshConnection,
}

#[derive(Clone)]
struct SshHostSelected {
    secondary: bool,
    host: SshHost,
}

#[derive(Clone, Debug)]
struct SshHostPickerDismissed;

struct SshHostPickerDelegate {
    selected_index: usize,
    hosts: Vec<SshHost>,
    matches: Vec<fuzzy_nucleo::StringMatch>,
}

impl SshHostPickerDelegate {
    fn new(cx: &App) -> Self {
        let hosts = collect_ssh_hosts(cx);
        let mut this = Self {
            selected_index: 0,
            hosts,
            matches: Vec::new(),
        };
        this.rebuild_matches("");
        this
    }

    fn selected_host(&self) -> Option<&SshHost> {
        self.matches
            .get(self.selected_index)
            .and_then(|matched| self.hosts.get(matched.candidate_id))
    }

    fn rebuild_matches(&mut self, query: &str) {
        use fuzzy_nucleo::StringMatchCandidate;
        use ordered_float::OrderedFloat;

        let candidates = self
            .hosts
            .iter()
            .enumerate()
            .map(|(id, host)| {
                let haystack = format!("{} {}", host.display_name, host.host_label);
                StringMatchCandidate::new(id, &haystack)
            })
            .collect::<Vec<_>>();

        let query = query.trim_start();
        let case = fuzzy_nucleo::Case::smart_if_uppercase_in(query);
        self.matches = fuzzy_nucleo::match_strings(
            &candidates,
            query,
            case,
            fuzzy_nucleo::LengthPenalty::On,
            100,
        );
        self.matches
            .sort_unstable_by_key(|matched| matched.candidate_id);

        self.selected_index = self
            .matches
            .iter()
            .enumerate()
            .rev()
            .max_by_key(|(_, matched)| OrderedFloat(matched.score))
            .map(|(index, _)| index)
            .unwrap_or(0);
    }
}

impl EventEmitter<SshHostSelected> for Picker<SshHostPickerDelegate> {}
impl EventEmitter<SshHostPickerDismissed> for Picker<SshHostPickerDelegate> {}

impl picker::PickerDelegate for SshHostPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "ssh-host-picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        Arc::from("Search SSH hosts")
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.rebuild_matches(&query);
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(host) = self.selected_host().cloned() {
            cx.emit(SshHostSelected { secondary, host });
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(SshHostPickerDismissed);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let matched = self.matches.get(ix)?;
        let host = self.hosts.get(matched.candidate_id)?;
        Some(
            ListItem::new(ix)
                .toggle_state(selected)
                .inset(true)
                .spacing(ui::ListItemSpacing::Sparse)
                .child(
                    h_flex()
                        .flex_grow_1()
                        .gap_3()
                        .min_w_0()
                        .child(Icon::new(IconName::Server).color(Color::Muted))
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap_1()
                                .child(HighlightedLabel::new(
                                    host.display_name.clone(),
                                    matched.positions.clone(),
                                ))
                                .child(
                                    Label::new(format!("({})", host.host_label))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        ),
                ),
        )
    }
}

pub struct SshHostPicker {
    create_new_window: bool,
    workspace: WeakEntity<Workspace>,
    picker: Entity<Picker<SshHostPickerDelegate>>,
    _subscriptions: [Subscription; 2],
}

impl SshHostPicker {
    pub fn new(
        create_new_window: bool,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = SshHostPickerDelegate::new(cx);
        let width = dialog_width(window) / window.rem_size();
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(rems(width))
                .embedded()
        });

        let selected = cx.subscribe_in(
            &picker,
            window,
            |this, _, event: &SshHostSelected, window, cx| {
                this.confirm(&event.host, event.secondary, window, cx);
            },
        );

        let dismissed = cx.subscribe_in(
            &picker,
            window,
            |this, _, _: &SshHostPickerDismissed, window, cx| {
                this.cancel(&menu::Cancel, window, cx);
            },
        );

        Self {
            create_new_window,
            workspace,
            picker,
            _subscriptions: [selected, dismissed],
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(
        &mut self,
        host: &SshHost,
        secondary: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let connection_options =
            RemoteConnectionOptions::Ssh(SshConnectionOptions::from(host.connection.clone()));
        let replace_current_window = match self.create_new_window {
            true => secondary,
            false => !secondary,
        };
        let open_mode = if replace_current_window {
            OpenMode::Activate
        } else {
            OpenMode::NewWindow
        };
        let requesting_window = if replace_current_window {
            window.window_handle().downcast::<MultiWorkspace>()
        } else {
            None
        };
        let open_options = OpenOptions {
            requesting_window,
            open_mode,
            ..Default::default()
        };

        let app_state = workspace.read(cx).app_state().clone();
        let fs = app_state.fs.clone();
        let identity = RemoteConnectionIdentity::from(&connection_options).into_owned();
        let workspace_handle = workspace.downgrade();

        cx.emit(DismissEvent);

        cx.spawn_in(window, async move |_, cx| {
            open_ssh_host(
                connection_options,
                identity,
                fs,
                app_state,
                open_options,
                workspace_handle,
                cx,
            )
            .await
        })
        .detach_and_prompt_err("Failed to open SSH host", window, cx, |_, _, _| None);
    }
}

impl EventEmitter<DismissEvent> for SshHostPicker {}

impl Focusable for SshHostPicker {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl ModalView for SshHostPicker {}

impl Render for SshHostPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(dialog_width(window))
            .overflow_hidden()
            .child(self.picker.clone())
    }
}

fn dialog_width(window: &Window) -> gpui::Pixels {
    let preferred = window.rem_size() * 34.;
    let available = (window.viewport_size().width - px(32.)).max(px(280.));
    preferred.min(available)
}

fn ssh_host_label(host: &str, username: Option<&str>) -> SharedString {
    match username {
        Some(user) if !user.is_empty() => format!("{user}@{host}").into(),
        _ => host.to_owned().into(),
    }
}

fn collect_ssh_hosts(cx: &App) -> Vec<SshHost> {
    let settings = RemoteSettings::get_global(cx);
    let mut hosts = Vec::new();
    let mut seen_aliases = BTreeSet::new();

    for connection in settings.ssh_connections() {
        seen_aliases.insert(connection.host.clone());
        let display_name = connection
            .nickname
            .clone()
            .unwrap_or_else(|| connection.host.clone());
        let host_label = ssh_host_label(&connection.host, connection.username.as_deref());
        hosts.push(SshHost {
            display_name: display_name.into(),
            host_label,
            connection,
        });
    }

    if settings.read_ssh_config {
        let mut hostname_by_alias = BTreeMap::new();
        let mut config_paths = vec![paths::user_ssh_config_file()];
        if let Some(global) = paths::global_ssh_config_file() {
            config_paths.push(global.to_path_buf());
        }
        for path in config_paths {
            if let Ok(content) = std::fs::read_to_string(&path) {
                for (alias, hostname, user) in parse_ssh_config_host_entries(&content) {
                    hostname_by_alias.entry(alias).or_insert((hostname, user));
                }
            }
        }

        for (alias, (hostname, user)) in hostname_by_alias {
            if seen_aliases.contains(&alias) {
                continue;
            }
            let resolved_host = hostname.unwrap_or_else(|| alias.clone());
            let host_label = ssh_host_label(&resolved_host, user.as_deref());
            hosts.push(SshHost {
                display_name: alias.clone().into(),
                host_label,
                connection: SshConnection {
                    host: alias,
                    username: user,
                    ..Default::default()
                },
            });
        }
    }

    hosts.sort_by(|left, right| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
    });
    hosts
}

fn latest_paths_for_identity(
    recent: &[RecentWorkspace],
    identity: &RemoteConnectionIdentity<'static>,
) -> Option<Vec<PathBuf>> {
    recent.iter().find_map(|entry| {
        let SerializedWorkspaceLocation::Remote(options) = &entry.location else {
            return None;
        };
        if &RemoteConnectionIdentity::from(options).into_owned() != identity {
            return None;
        }
        let paths = entry.paths.paths().to_vec();
        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    })
}

fn latest_username_for_host_port(
    recent: &[RecentWorkspace],
    host: &str,
    port: Option<u16>,
) -> Option<String> {
    recent.iter().find_map(|entry| {
        let SerializedWorkspaceLocation::Remote(RemoteConnectionOptions::Ssh(options)) =
            &entry.location
        else {
            return None;
        };
        if options.host.to_cow().as_ref() != host || options.port != port {
            return None;
        }
        options.username.clone()
    })
}

fn fill_username_from_recent(
    connection_options: &mut RemoteConnectionOptions,
    identity: &mut RemoteConnectionIdentity<'static>,
    recent: &[RecentWorkspace],
) {
    let username = {
        let RemoteConnectionOptions::Ssh(ssh) = &*connection_options else {
            return;
        };
        if ssh.username.is_some() {
            return;
        }
        let host = ssh.host.to_string();
        latest_username_for_host_port(recent, &host, ssh.port)
    };
    let Some(username) = username else {
        return;
    };
    if let RemoteConnectionOptions::Ssh(ssh) = connection_options {
        ssh.username = Some(username);
    }
    *identity = RemoteConnectionIdentity::from(&*connection_options).into_owned();
}

async fn open_ssh_host(
    mut connection_options: RemoteConnectionOptions,
    mut identity: RemoteConnectionIdentity<'static>,
    fs: Arc<dyn Fs>,
    app_state: Arc<workspace::AppState>,
    open_options: OpenOptions,
    workspace: WeakEntity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<()> {
    let db = cx.update(|_, cx| WorkspaceDb::global(cx))?;
    let recent = db
        .recent_project_workspaces(fs.as_ref())
        .await
        .unwrap_or_default();

    fill_username_from_recent(&mut connection_options, &mut identity, &recent);

    if let Some(paths) = latest_paths_for_identity(&recent, &identity)
        && open_remote_project(
            connection_options.clone(),
            paths,
            app_state.clone(),
            open_options.clone(),
            cx,
        )
        .await
        .log_err()
        .is_some()
    {
        return Ok(());
    }

    open_remote_home(connection_options, app_state, open_options, workspace, cx).await
}

async fn open_remote_home(
    connection_options: RemoteConnectionOptions,
    app_state: Arc<workspace::AppState>,
    open_options: OpenOptions,
    workspace: WeakEntity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<()> {
    let create_new_window = matches!(open_options.open_mode, OpenMode::NewWindow);
    let window = if create_new_window {
        if let Some(source) = workspace.upgrade() {
            source
                .update_in(cx, |workspace, window, cx| {
                    workspace.hide_modal(window, cx);
                })
                .ok();
        }
        let options = cx.update(|_, cx| (app_state.build_window_options)(None, cx))?;
        let window = cx.open_window(options, {
            let app_state = app_state.clone();
            move |window, cx| {
                let project = project::Project::local(
                    app_state.client.clone(),
                    app_state.node_runtime.clone(),
                    app_state.user_store.clone(),
                    app_state.languages.clone(),
                    app_state.fs.clone(),
                    None,
                    project::LocalProjectFlags {
                        init_worktree_trust: false,
                        ..Default::default()
                    },
                    cx,
                );
                let workspace = cx.new(|cx| {
                    Workspace::new(None, project, app_state.clone(), window, cx)
                });
                cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
            }
        })?;
        window
            .update(cx, |_, window, _| window.activate_window())
            .ok();
        window
    } else {
        if workspace.upgrade().is_none() {
            anyhow::bail!("workspace closed");
        }
        cx.window_handle()
            .downcast::<MultiWorkspace>()
            .ok_or_else(|| anyhow::anyhow!("expected MultiWorkspace window"))?
    };

    let connected = window.update(cx, {
        let app_state = app_state.clone();
        let connection_options = connection_options.clone();
        move |multi_workspace, window, cx| {
            window.activate_window();
            let workspace = multi_workspace.workspace().clone();
            workspace.update(cx, |workspace, cx| {
                workspace.hide_modal(window, cx);
                workspace.toggle_modal(window, cx, |window, cx| {
                    RemoteConnectionModal::new(&connection_options, Vec::new(), window, cx)
                });
                let Some(modal) = workspace.active_modal::<RemoteConnectionModal>(cx) else {
                    return Task::ready(Err(anyhow::anyhow!("connection modal unavailable")));
                };
                let prompt = modal.read(cx).prompt.clone();
                let connect_task = connect(
                    ConnectionIdentifier::setup(),
                    connection_options.clone(),
                    prompt,
                    window,
                    cx,
                );
                cx.spawn_in(window, async move |workspace, cx| {
                    let Some(session) = connect_task.await? else {
                        anyhow::bail!("SSH connection cancelled");
                    };
                    workspace.update(cx, |workspace, cx| {
                        if let Some(prompt) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                            prompt.update(cx, |prompt, cx| prompt.finished(cx))
                        }
                    })?;
                    let project = cx.update(|_, cx| {
                        project::Project::remote(
                            session.clone(),
                            app_state.client.clone(),
                            app_state.node_runtime.clone(),
                            app_state.user_store.clone(),
                            app_state.languages.clone(),
                            app_state.fs.clone(),
                            true,
                            cx,
                        )
                    })?;
                    Ok((session, project))
                })
            })
        }
    });
    let (session, project) = match connected {
        Ok(task) => match task.await {
            Ok(connected) => connected,
            Err(error) => {
                if create_new_window {
                    window
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                }
                return Err(error);
            }
        },
        Err(error) => {
            if create_new_window {
                window
                    .update(cx, |_, window, _| window.remove_window())
                    .ok();
            }
            return Err(error);
        }
    };

    let path_style = cx.update(|_, cx| session.read(cx).path_style())?;
    let home_dir = project
        .read_with(cx, |project, cx| project.resolve_abs_path("~", cx))
        .await
        .and_then(|path| path.into_abs_path())
        .map(PathBuf::from)
        .unwrap_or_else(|| match path_style {
            PathStyle::Unix => PathBuf::from("/"),
            PathStyle::Windows => PathBuf::from("C:\\"),
        });

    open_remote_project_with_existing_connection(
        connection_options,
        project,
        vec![home_dir],
        app_state,
        window,
        None,
        Some(workspace),
        cx,
    )
    .await?;
    Ok(())
}
