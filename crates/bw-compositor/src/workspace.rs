//! ext-workspace-v1 with one workspace group holding one always-active workspace, so pagers (the Xfce
//! pager through libxfce4windowing, waybar's `ext/workspaces`) show an entry instead of a placeholder.
//! Nothing can be created, removed or switched: the capabilities are empty and the requests are ignored.

use smithay::reexports::{
    wayland_protocols::ext::workspace::v1::server::{
        ext_workspace_group_handle_v1::{self as group, ExtWorkspaceGroupHandleV1},
        ext_workspace_handle_v1::{self as handle, ExtWorkspaceHandleV1},
        ext_workspace_manager_v1::{self as manager, ExtWorkspaceManagerV1},
    },
    wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, backend::ClientId, protocol::wl_output::WlOutput},
};

use crate::State;

pub const VERSION: u32 = 1;

/// The live groups, so a client that binds `wl_output` after the manager still gets `output_enter`.
#[derive(Default)]
pub struct Workspaces {
    groups: Vec<(ExtWorkspaceManagerV1, ExtWorkspaceGroupHandleV1)>,
}

impl Workspaces {
    pub fn output_bound(&mut self, wl_output: &WlOutput) {
        for (manager, group) in self.groups.iter().filter(|(m, _)| m.client().is_some_and(|c| Some(c) == wl_output.client())) {
            group.output_enter(wl_output);
            manager.done();
        }
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for State {
    fn bind(state: &mut Self, dh: &DisplayHandle, client: &Client, resource: New<ExtWorkspaceManagerV1>, _: &(), data_init: &mut DataInit<'_, Self>) {
        let manager = data_init.init(resource, ());
        let (Ok(group), Ok(ws)) = (
            client.create_resource::<ExtWorkspaceGroupHandleV1, (), State>(dh, manager.version(), ()),
            client.create_resource::<ExtWorkspaceHandleV1, (), State>(dh, manager.version(), ()),
        ) else {
            return;
        };
        manager.workspace_group(&group);
        group.capabilities(group::GroupCapabilities::empty());
        for o in state.output.client_outputs(client) {
            group.output_enter(&o);
        }
        manager.workspace(&ws);
        ws.id("1".into());
        ws.name("1".into());
        ws.state(handle::State::Active);
        ws.capabilities(handle::WorkspaceCapabilities::empty());
        group.workspace_enter(&ws);
        manager.done();
        state.workspaces.groups.push((manager, group));
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn request(_: &mut Self, _: &Client, manager: &ExtWorkspaceManagerV1, request: manager::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {
        if let manager::Request::Stop = request {
            manager.finished();
        }
        // commit: nothing was requested that we would act on
    }
    fn destroyed(state: &mut Self, _: ClientId, manager: &ExtWorkspaceManagerV1, _: &()) {
        state.workspaces.groups.retain(|(m, _)| m != manager);
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
    fn request(_: &mut Self, _: &Client, _: &ExtWorkspaceGroupHandleV1, _: group::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
    fn destroyed(state: &mut Self, _: ClientId, group: &ExtWorkspaceGroupHandleV1, _: &()) {
        state.workspaces.groups.retain(|(_, g)| g != group);
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for State {
    fn request(_: &mut Self, _: &Client, _: &ExtWorkspaceHandleV1, _: handle::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}
