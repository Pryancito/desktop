// SPDX-License-Identifier: AGPL-3.0-only
//! `zwlr_foreign_toplevel_management_unstable_v1` and `ext_workspace_v1`:
//! window and workspace control for a third-party taskbar. Eclipse OS fork
//! addition (Pryancito/desktop), so that the Eclipse OS panel (lunarbar) and
//! Waybar work under Abyss unchanged.
//!
//! Upstream keeps window control off ambient globals (COMP-06 §2), and this
//! file keeps that rule's spirit: both managers are visible only to the
//! executables named in `toplevel-management { allow ... }` in `policy.kdl`,
//! the same named-admission model as `virtual-pointer` (ADR 0094). Empty, the
//! default, hides both from everyone. Two more limits apply on top:
//!
//! - a `secret` window is never published (the same rule as
//!   `foreign_toplevel.rs`, which is what the read-only list uses);
//! - while the session is locked or a trusted prompt is up, every request is
//!   ignored. `activate` would be refused anyway (`focus_window_raising`);
//!   close, minimize, maximize and fullscreen are refused here.
//!
//! State changes are not pushed from every place that makes them. Instead
//! [`flush`] runs once per event-loop iteration (from each backend, next to
//! `protocols::agent::flush`), recomputes what every client should see and
//! sends only the difference. With a handful of windows that is cheap, and it
//! cannot miss a change that some new code path makes without telling us.

use smithay::{
    desktop::Window,
    reexports::{
        wayland_protocols::ext::workspace::v1::server::{
            ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
            ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, WorkspaceCapabilities},
            ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
        },
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
            zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
        },
        wayland_server::{
            backend::{ClientId, GlobalId},
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
        },
    },
    utils::IsAlive,
};

use crate::{
    config::ExeAllowlist,
    protocols::standard::data_control::{admit, WeakDh},
    shell::workspace::COUNT,
    state::AbyssState,
};

const TOPLEVEL_VERSION: u32 = 3;
const WORKSPACE_VERSION: u32 = 1;

/// Workspaces past this many are `hidden` unless they are active or hold a
/// window, as the `de` pager cuts its grid to the desktops in use. labwc's
/// Eclipse OS config has four desktops, so lunarbar shows 01-04 as before.
const ALWAYS_SHOWN: usize = 4;

/// What a client last saw of one window.
#[derive(Clone, PartialEq, Default)]
struct Seen {
    title: String,
    app_id: String,
    maximized: bool,
    minimized: bool,
    activated: bool,
    fullscreen: bool,
    output: Option<u64>,
}

impl Seen {
    fn state_bytes(&self) -> Vec<u8> {
        use zwlr_foreign_toplevel_handle_v1::State;
        let mut out = Vec::new();
        for (on, s) in [
            (self.maximized, State::Maximized),
            (self.minimized, State::Minimized),
            (self.activated, State::Activated),
            (self.fullscreen, State::Fullscreen),
        ] {
            if on {
                out.extend_from_slice(&(s as u32).to_ne_bytes());
            }
        }
        out
    }
}

/// One published window: its handle on every manager that has seen it.
struct Published {
    window: Window,
    seen: Seen,
    handles: Vec<ZwlrForeignToplevelHandleV1>,
}

/// One `ext_workspace_manager_v1` and everything it was told.
struct WsClient {
    manager: ExtWorkspaceManagerV1,
    /// `(output id, group)`.
    groups: Vec<(u64, ExtWorkspaceGroupHandleV1)>,
    /// `(output id, index, handle, state last sent)`.
    workspaces: Vec<(u64, usize, ExtWorkspaceHandleV1, u32)>,
    /// Activations requested since the last `commit`.
    pending: Vec<(u64, usize)>,
}

/// Global data for both managers: enough to identify a client at bind time.
#[derive(Debug)]
pub struct ManagerData {
    dh: WeakDh,
    allow: ExeAllowlist,
}

/// User data on a toplevel handle: the window it names.
pub struct HandleData {
    window: Window,
}

/// User data on a workspace handle.
pub struct WsData {
    output: u64,
    index: usize,
}

pub struct ToplevelManagement {
    #[allow(dead_code)] // holds the globals alive
    globals: [GlobalId; 2],
    /// Who may bind. Shared with the globals' filters; the config reload path
    /// writes it.
    pub allow: ExeAllowlist,
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    published: Vec<Published>,
    ws_clients: Vec<WsClient>,
}

impl ToplevelManagement {
    pub fn new(display: &DisplayHandle, dh: WeakDh, allow: ExeAllowlist) -> Self {
        let toplevel = display.create_global::<AbyssState, ZwlrForeignToplevelManagerV1, _>(
            TOPLEVEL_VERSION,
            ManagerData {
                dh: dh.clone(),
                allow: allow.clone(),
            },
        );
        let workspace = display.create_global::<AbyssState, ExtWorkspaceManagerV1, _>(
            WORKSPACE_VERSION,
            ManagerData {
                dh,
                allow: allow.clone(),
            },
        );
        Self {
            globals: [toplevel, workspace],
            allow,
            managers: Vec::new(),
            published: Vec::new(),
            ws_clients: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------- sync ----

/// Bring every bound manager up to date. Called once per event-loop
/// iteration, before the clients are flushed.
pub fn flush(state: &mut AbyssState) {
    if !state.toplevel_management.managers.is_empty() || !state.toplevel_management.published.is_empty() {
        flush_toplevels(state);
    }
    if !state.toplevel_management.ws_clients.is_empty() {
        flush_workspaces(state);
    }
}

/// The windows a taskbar may see, with what it should see of each.
fn visible_windows(state: &AbyssState) -> Vec<(Window, Seen)> {
    let focus = state.focus.clone();
    crate::shell::owned_windows(state)
        .into_iter()
        .filter(|w| w.alive() && !crate::shell::is_unmanaged_x11(state, w))
        .filter(|w| !crate::protocols::standard::foreign_toplevel::is_secret(state, w))
        .map(|w| {
            let (app_id, title) = crate::ipc::methods::identity_of(&w);
            let seen = Seen {
                title: title.unwrap_or_default(),
                app_id: app_id.unwrap_or_default(),
                maximized: crate::shell::maximize::is_maximized(state, &w),
                minimized: crate::shell::is_minimized(state, &w),
                activated: focus.as_ref() == Some(&w),
                fullscreen: state.fullscreen.contains_key(&w),
                output: crate::shell::output_of_window(state, &w),
            };
            (w, seen)
        })
        .collect()
}

fn flush_toplevels(state: &mut AbyssState) {
    let now = if state.toplevel_management.managers.is_empty() {
        Vec::new()
    } else {
        visible_windows(state)
    };
    let dh = state.display_handle.clone();
    // Gone (closed, or now `secret`): `closed` on every handle.
    state.toplevel_management.published.retain(|p| {
        let keep = now.iter().any(|(w, _)| *w == p.window);
        if !keep {
            for h in &p.handles {
                h.closed();
            }
        }
        keep
    });
    for (window, seen) in now {
        let idx = match state
            .toplevel_management
            .published
            .iter()
            .position(|p| p.window == window)
        {
            Some(i) => i,
            None => {
                state.toplevel_management.published.push(Published {
                    window: window.clone(),
                    seen: Seen::default(),
                    handles: Vec::new(),
                });
                state.toplevel_management.published.len() - 1
            }
        };
        let managers = state.toplevel_management.managers.clone();
        let p = &mut state.toplevel_management.published[idx];
        // A handle on every manager that has none yet: a new window, or a
        // manager bound since the last flush. Each gets the full state.
        for m in &managers {
            let Some(client) = m.client() else { continue };
            if p.handles.iter().any(|h| h.client().as_ref() == Some(&client)) {
                continue;
            }
            let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, AbyssState>(
                &dh,
                m.version(),
                HandleData {
                    window: window.clone(),
                },
            ) else {
                continue;
            };
            m.toplevel(&handle);
            handle.title(seen.title.clone());
            handle.app_id(seen.app_id.clone());
            send_output(state_outputs(&state.outputs, seen.output), &client, &handle, true);
            handle.state(seen.state_bytes());
            handle.done();
            p.handles.push(handle);
        }
        if p.seen != seen {
            for h in &p.handles {
                let Some(client) = h.client() else { continue };
                if p.seen.title != seen.title {
                    h.title(seen.title.clone());
                }
                if p.seen.app_id != seen.app_id {
                    h.app_id(seen.app_id.clone());
                }
                if p.seen.output != seen.output {
                    send_output(state_outputs(&state.outputs, p.seen.output), &client, h, false);
                    send_output(state_outputs(&state.outputs, seen.output), &client, h, true);
                }
                h.state(seen.state_bytes());
                h.done();
            }
            p.seen = seen;
        }
    }
}

fn state_outputs(outputs: &crate::outputs::Outputs, id: Option<u64>) -> Option<smithay::output::Output> {
    id.and_then(|id| outputs.get(id)).map(|e| e.output.clone())
}

fn send_output(
    output: Option<smithay::output::Output>,
    client: &Client,
    handle: &ZwlrForeignToplevelHandleV1,
    enter: bool,
) {
    let Some(output) = output else { return };
    for wl in output.client_outputs(client) {
        if enter {
            handle.output_enter(&wl);
        } else {
            handle.output_leave(&wl);
        }
    }
}

/// The `ext_workspace_handle_v1` state each workspace should show, per output.
fn workspace_states(state: &AbyssState) -> Vec<(u64, String, Vec<u32>)> {
    use ext_workspace_handle_v1::State;
    state
        .outputs
        .iter()
        .map(|e| {
            let states = (0..COUNT)
                .map(|i| {
                    let active = i == e.active;
                    let empty = e.workspaces.get(i).is_none_or(|ws| ws.all_windows().is_empty());
                    let mut s = State::empty();
                    if active {
                        s |= State::Active;
                    }
                    if i >= ALWAYS_SHOWN && !active && empty {
                        s |= State::Hidden;
                    }
                    s.bits()
                })
                .collect();
            (e.id, e.identity.clone(), states)
        })
        .collect()
}

fn flush_workspaces(state: &mut AbyssState) {
    let want = workspace_states(state);
    let dh = state.display_handle.clone();
    let outputs: Vec<(u64, smithay::output::Output)> =
        state.outputs.iter().map(|e| (e.id, e.output.clone())).collect();
    for c in state.toplevel_management.ws_clients.iter_mut() {
        let Some(client) = c.manager.client() else {
            continue;
        };
        let mut changed = false;
        // Outputs that went away: their workspaces, then the group.
        c.workspaces.retain(|(out, _, ws, _)| {
            let keep = want.iter().any(|(id, _, _)| id == out);
            if !keep {
                ws.removed();
                changed = true;
            }
            keep
        });
        c.groups.retain(|(out, g)| {
            let keep = want.iter().any(|(id, _, _)| id == out);
            if !keep {
                g.removed();
                changed = true;
            }
            keep
        });
        for (out, identity, states) in &want {
            if !c.groups.iter().any(|(o, _)| o == out) {
                let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, AbyssState>(
                    &dh,
                    c.manager.version(),
                    *out,
                ) else {
                    continue;
                };
                c.manager.workspace_group(&group);
                group.capabilities(GroupCapabilities::empty());
                if let Some((_, o)) = outputs.iter().find(|(id, _)| id == out) {
                    for wl in o.client_outputs(&client) {
                        group.output_enter(&wl);
                    }
                }
                c.groups.push((*out, group));
                changed = true;
            }
            let group = c.groups.iter().find(|(o, _)| o == out).map(|(_, g)| g.clone());
            for (i, bits) in states.iter().enumerate() {
                match c
                    .workspaces
                    .iter_mut()
                    .find(|(o, idx, _, _)| o == out && *idx == i)
                {
                    Some((_, _, ws, last)) => {
                        if last != bits {
                            ws.state(ext_workspace_handle_v1::State::from_bits_truncate(*bits));
                            *last = *bits;
                            changed = true;
                        }
                    }
                    None => {
                        let Ok(ws) = client.create_resource::<ExtWorkspaceHandleV1, _, AbyssState>(
                            &dh,
                            c.manager.version(),
                            WsData {
                                output: *out,
                                index: i,
                            },
                        ) else {
                            continue;
                        };
                        c.manager.workspace(&ws);
                        ws.id(format!("{identity}/{}", i + 1));
                        ws.name(format!("{}", i + 1));
                        ws.coordinates((i as u32).to_ne_bytes().to_vec());
                        ws.capabilities(WorkspaceCapabilities::Activate);
                        ws.state(ext_workspace_handle_v1::State::from_bits_truncate(*bits));
                        if let Some(g) = &group {
                            g.workspace_enter(&ws);
                        }
                        c.workspaces.push((*out, i, ws, *bits));
                        changed = true;
                    }
                }
            }
        }
        if changed {
            c.manager.done();
        }
    }
}

/// Whether a request from a taskbar may act now.
fn may_act(state: &AbyssState) -> bool {
    !state.lock.locked && !state.trusted_ui.active()
}

/// `activate`: bring the window back wherever it is (minimized, another
/// workspace, another output) and focus it.
fn activate(state: &mut AbyssState, window: &Window) {
    if crate::shell::is_minimized(state, window) {
        crate::shell::set_minimized(state, window, false);
    }
    let place = state.outputs.iter().find_map(|e| {
        e.workspaces
            .iter()
            .position(|ws| ws.holds(window))
            .map(|i| (e.id, i, e.active))
    });
    if let Some((out, idx, active)) = place {
        if idx != active {
            state.outputs.set_focused(out);
            crate::shell::switch_workspace(state, idx + 1);
        }
    }
    crate::shell::focus_window(state, window);
}

fn close(window: &Window) {
    match window.underlying_surface() {
        smithay::desktop::WindowSurface::Wayland(t) => t.send_close(),
        smithay::desktop::WindowSurface::X11(x) => {
            let _ = x.close();
        }
    }
}

fn set_fullscreen(
    state: &mut AbyssState,
    window: &Window,
    on: bool,
    output: Option<&smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
) {
    if let Some(toplevel) = window.toplevel().cloned() {
        if on {
            crate::shell::fullscreen_toplevel(state, &toplevel, output);
        } else {
            crate::shell::unfullscreen_toplevel(state, &toplevel);
        }
    } else if window.x11_surface().is_some() {
        crate::shell::set_x11_fullscreen(state, window, on);
    }
}

// --------------------------------------------------- foreign toplevel ----

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ManagerData> for AbyssState {
    fn bind(
        state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        manager: New<ZwlrForeignToplevelManagerV1>,
        _data: &ManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(manager, ());
        // The handles follow on the next flush, in the same dispatch round.
        state.toplevel_management.managers.push(manager);
    }

    fn can_view(client: Client, data: &ManagerData) -> bool {
        admit(
            &data.dh,
            &data.allow,
            &client,
            "zwlr_foreign_toplevel_manager_v1",
            "toplevel-management.allow",
        )
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for AbyssState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            state.toplevel_management.managers.retain(|m| m != manager);
            manager.finished();
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, manager: &ZwlrForeignToplevelManagerV1, _data: &()) {
        state.toplevel_management.managers.retain(|m| m != manager);
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, HandleData> for AbyssState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &HandleData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;
        let window = data.window.clone();
        if !window.alive() || !may_act(state) {
            return;
        }
        match request {
            Request::Activate { .. } => activate(state, &window),
            Request::Close => close(&window),
            Request::SetMinimized => crate::shell::set_minimized(state, &window, true),
            Request::UnsetMinimized => crate::shell::set_minimized(state, &window, false),
            Request::SetMaximized => {
                crate::shell::maximize::set(state, &window, Some((true, true)));
            }
            Request::UnsetMaximized => {
                crate::shell::maximize::set(state, &window, None);
            }
            Request::SetFullscreen { output } => set_fullscreen(state, &window, true, output.as_ref()),
            Request::UnsetFullscreen => set_fullscreen(state, &window, false, None),
            // Only a hint for a minimize animation; the bar's chip rect over
            // IPC already does that job here.
            Request::SetRectangle { .. } | Request::Destroy => return,
            _ => return,
        }
        crate::backend::damage_all(state);
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        handle: &ZwlrForeignToplevelHandleV1,
        _data: &HandleData,
    ) {
        for p in state.toplevel_management.published.iter_mut() {
            p.handles.retain(|h| h != handle);
        }
    }
}

// ----------------------------------------------------------- workspace ----

impl GlobalDispatch<ExtWorkspaceManagerV1, ManagerData> for AbyssState {
    fn bind(
        state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        manager: New<ExtWorkspaceManagerV1>,
        _data: &ManagerData,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(manager, ());
        state.toplevel_management.ws_clients.push(WsClient {
            manager,
            groups: Vec::new(),
            workspaces: Vec::new(),
            pending: Vec::new(),
        });
    }

    fn can_view(client: Client, data: &ManagerData) -> bool {
        admit(
            &data.dh,
            &data.allow,
            &client,
            "ext_workspace_manager_v1",
            "toplevel-management.allow",
        )
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for AbyssState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use ext_workspace_manager_v1::Request;
        let Some(c) = state
            .toplevel_management
            .ws_clients
            .iter_mut()
            .find(|c| &c.manager == manager)
        else {
            return;
        };
        match request {
            Request::Commit => {
                let pending = std::mem::take(&mut c.pending);
                if !may_act(state) {
                    return;
                }
                for (out, idx) in pending {
                    if state.outputs.get(out).is_some() {
                        state.outputs.set_focused(out);
                        crate::shell::switch_workspace(state, idx + 1);
                    }
                }
                crate::backend::damage_all(state);
            }
            Request::Stop => {
                manager.finished();
                state
                    .toplevel_management
                    .ws_clients
                    .retain(|c| &c.manager != manager);
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, manager: &ExtWorkspaceManagerV1, _data: &()) {
        state
            .toplevel_management
            .ws_clients
            .retain(|c| &c.manager != manager);
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, u64> for AbyssState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _group: &ExtWorkspaceGroupHandleV1,
        _request: ext_workspace_group_handle_v1::Request,
        _data: &u64,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // `create_workspace` is not advertised (the set is fixed at
        // `workspace::COUNT`); `destroy` needs nothing from us.
    }

    fn destroyed(state: &mut Self, _client: ClientId, group: &ExtWorkspaceGroupHandleV1, _data: &u64) {
        for c in state.toplevel_management.ws_clients.iter_mut() {
            c.groups.retain(|(_, g)| g != group);
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, WsData> for AbyssState {
    fn request(
        state: &mut Self,
        _client: &Client,
        ws: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &WsData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let ext_workspace_handle_v1::Request::Activate = request {
            if let Some(c) = state
                .toplevel_management
                .ws_clients
                .iter_mut()
                .find(|c| c.workspaces.iter().any(|(_, _, w, _)| w == ws))
            {
                c.pending.push((data.output, data.index));
            }
        }
        // `deactivate`, `assign` and `remove` are not advertised.
    }

    fn destroyed(state: &mut Self, _client: ClientId, ws: &ExtWorkspaceHandleV1, _data: &WsData) {
        for c in state.toplevel_management.ws_clients.iter_mut() {
            c.workspaces.retain(|(_, _, w, _)| w != ws);
        }
    }
}
