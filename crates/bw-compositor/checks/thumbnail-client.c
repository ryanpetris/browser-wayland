// Controlled Wayland surfaces for the Docker thumbnail check. Commands arrive through a text file.
#define _GNU_SOURCE
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include <sys/mman.h>
#include <unistd.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static struct wl_display *display;
static struct wl_compositor *compositor;
static struct wl_subcompositor *subcompositor;
static struct wl_shm *shm;
static struct xdg_wm_base *wm;
static struct wl_surface *root, *child, *popup_surface;
static struct wl_subsurface *subsurface, *popup_subsurface;
static struct wl_surface *popup_child;
static struct xdg_surface *shell, *popup_shell;
static struct xdg_popup *popup;
static int width = 400, height = 300, running = 1;
static uint32_t color = 0xff202080;

static void paint(struct wl_surface *surface, int w, int h, uint32_t value) {
    int fd = memfd_create("thumbnail-pixels", 0);
    if (fd < 0 || ftruncate(fd, w*h*4)) exit(2);
    uint32_t *pixels = mmap(NULL, w*h*4, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0);
    if (pixels == MAP_FAILED) exit(2);
    for (int i = 0; i < w*h; i++) pixels[i] = value;
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, w*h*4);
    struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0, w, h, w*4, WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool); munmap(pixels, w*h*4); close(fd);
    wl_surface_attach(surface, buffer, 0, 0);
    wl_surface_damage(surface, 0, 0, w, h);
    wl_surface_commit(surface);
    // The fixture makes a bounded number of buffers; display disconnect releases them.
}
static void configure(void *data, struct xdg_surface *surface, uint32_t serial) {
    xdg_surface_ack_configure(surface, serial);
    if (surface == shell) paint(root, width, height, color);
    else paint(popup_surface, 80, 80, 0xffe02020);
}
static const struct xdg_surface_listener shell_listener = { .configure = configure };
static void size(void *data, struct xdg_toplevel *top, int32_t w, int32_t h, struct wl_array *states) {
    if (w > 0) width = w;
    if (h > 0) height = h;
}
static void close_window(void *data, struct xdg_toplevel *top) { running = 0; }
static const struct xdg_toplevel_listener top_listener = { .configure = size, .close = close_window };
static void ping(void *data, struct xdg_wm_base *base, uint32_t serial) { xdg_wm_base_pong(base, serial); }
static const struct xdg_wm_base_listener wm_listener = { .ping = ping };
static void registry_global(void *data, struct wl_registry *registry, uint32_t id, const char *name, uint32_t version) {
    if (!strcmp(name, "wl_compositor")) compositor = wl_registry_bind(registry, id, &wl_compositor_interface, 1);
    if (!strcmp(name, "wl_subcompositor")) subcompositor = wl_registry_bind(registry, id, &wl_subcompositor_interface, 1);
    if (!strcmp(name, "wl_shm")) shm = wl_registry_bind(registry, id, &wl_shm_interface, 1);
    if (!strcmp(name, "xdg_wm_base")) wm = wl_registry_bind(registry, id, &xdg_wm_base_interface, 1);
}
static void registry_remove(void *data, struct wl_registry *registry, uint32_t id) {}
static const struct wl_registry_listener registry_listener = { registry_global, registry_remove };
static void remove_popup(void) {
    if (!popup) return;
    if (popup_child) { wl_subsurface_destroy(popup_subsurface); wl_surface_destroy(popup_child); popup_child = NULL; }
    xdg_popup_destroy(popup); xdg_surface_destroy(popup_shell); wl_surface_destroy(popup_surface);
    popup = NULL;
}
static void popup_configure(void *data, struct xdg_popup *p, int32_t x, int32_t y, int32_t w, int32_t h) {}
static void popup_done(void *data, struct xdg_popup *p) { remove_popup(); }
static const struct xdg_popup_listener popup_listener = { .configure = popup_configure, .popup_done = popup_done };
int main(int argc, char **argv) {
    if (argc != 2 || !(display = wl_display_connect(NULL))) return 2;
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    wl_display_roundtrip(display);
    if (!compositor || !subcompositor || !shm || !wm) return 2;
    xdg_wm_base_add_listener(wm, &wm_listener, NULL);
    root = wl_compositor_create_surface(compositor);
    shell = xdg_wm_base_get_xdg_surface(wm, root);
    xdg_surface_add_listener(shell, &shell_listener, NULL);
    struct xdg_toplevel *top = xdg_surface_get_toplevel(shell);
    xdg_toplevel_add_listener(top, &top_listener, NULL);
    xdg_toplevel_set_app_id(top, "thumbnail-surfaces");
    xdg_toplevel_set_title(top, "Thumbnail surfaces");
    wl_surface_commit(root);
    char previous[100] = "";
    while (running) {
        wl_display_dispatch_pending(display); wl_display_flush(display);
        struct pollfd fd = { .fd = wl_display_get_fd(display), .events = POLLIN };
        if (poll(&fd, 1, 20) > 0 && wl_display_dispatch(display) < 0) break;
        FILE *file = fopen(argv[1], "r");
        char command[100] = "";
        if (file) { fgets(command, sizeof command, file); fclose(file); }
        if (!command[0] || !strcmp(previous, command)) continue;
        strcpy(previous, command);
        uint32_t value;
        if (sscanf(command, "root %x", &value) == 1) { color = value; paint(root, width, height, color); }
        if (sscanf(command, "sub %x", &value) == 1) {
            if (!child) {
                child = wl_compositor_create_surface(compositor);
                subsurface = wl_subcompositor_get_subsurface(subcompositor, child, root);
                wl_subsurface_set_position(subsurface, 40, 40);
            }
            paint(child, 100, 100, value); wl_surface_commit(root);
        }
        if (sscanf(command, "async %x", &value) == 1 && child) {
            wl_subsurface_set_desync(subsurface); paint(child, 100, 100, value);
        }
        if (!strcmp(command, "burst")) {
            struct timespec now; clock_gettime(CLOCK_MONOTONIC, &now);
            if (now.tv_nsec > 500000000) usleep((1000000000 - now.tv_nsec) / 1000);
            color = 0xffc08020; paint(root, width, height, color);
            wl_display_roundtrip(display); usleep(50000);
            color = 0xff20c080; paint(root, width, height, color);
        }
        if (!strcmp(command, "sub-off") && child) {
            wl_subsurface_destroy(subsurface); wl_surface_destroy(child); child = NULL;
        }
        if (!strcmp(command, "popup") && !popup) {
            struct xdg_positioner *positioner = xdg_wm_base_create_positioner(wm);
            xdg_positioner_set_size(positioner, 80, 80);
            xdg_positioner_set_anchor_rect(positioner, 180, 100, 1, 1);
            popup_surface = wl_compositor_create_surface(compositor);
            popup_shell = xdg_wm_base_get_xdg_surface(wm, popup_surface);
            xdg_surface_add_listener(popup_shell, &shell_listener, NULL);
            popup = xdg_surface_get_popup(popup_shell, shell, positioner);
            xdg_popup_add_listener(popup, &popup_listener, NULL);
            xdg_positioner_destroy(positioner); wl_surface_commit(popup_surface);
        }
        if (sscanf(command, "popup-sub %x", &value) == 1 && popup) {
            if (!popup_child) {
                popup_child = wl_compositor_create_surface(compositor);
                popup_subsurface = wl_subcompositor_get_subsurface(subcompositor, popup_child, popup_surface);
                wl_subsurface_set_position(popup_subsurface, 20, 20);
                wl_subsurface_set_desync(popup_subsurface);
            }
            paint(popup_child, 40, 40, value);
        }
        if (!strcmp(command, "popup-off")) remove_popup();
    }
    wl_display_disconnect(display);
    return 0;
}
