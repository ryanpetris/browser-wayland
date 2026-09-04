//! ext-workspace-v1 with one workspace group holding one always-active workspace, so pagers (the Xfce
//! pager through libxfce4windowing, waybar's `ext/workspaces`) show an entry instead of a placeholder.
//! Nothing can be created, removed or switched: the capabilities are empty and the requests are ignored.

use smithay::reexports::{
    wayland_protocols::ext::workspace::v1::server::{
        ext_workspace_group_handle_v1::{self as group, ExtWorkspaceGroupHandleV1},
        ext_workspace_handle_v1::{self as handle, ExtWorkspaceHandleV1},
        ext_workspace_manager_v1::{self as manager, ExtWorkspaceManagerV1},
    },
    wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource},
};

use crate::State;

pub const VERSION: u32 = 1;

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
        ws.coordinates(vec![0, 0, 0, 0]); // one u32 coordinate: position 0 in the group
        ws.state(handle::State::Active);
        ws.capabilities(handle::WorkspaceCapabilities::empty());
        group.workspace_enter(&ws);
        manager.done();
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn request(_: &mut Self, _: &Client, manager: &ExtWorkspaceManagerV1, request: manager::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {
        if let manager::Request::Stop = request {
            manager.finished();
        }
        // commit: nothing was requested that we would act on
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
    fn request(_: &mut Self, _: &Client, _: &ExtWorkspaceGroupHandleV1, _: group::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for State {
    fn request(_: &mut Self, _: &Client, _: &ExtWorkspaceHandleV1, _: handle::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, Self>) {}
}
