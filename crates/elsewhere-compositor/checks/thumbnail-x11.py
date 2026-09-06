# Run with GDK_BACKEND=x11 in the Docker rig; the command file controls painted content.
import sys
import gi

gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, GLib

window = Gtk.Window(title='Thumbnail X11')
window.set_wmclass('thumbnail-x11', 'thumbnail-x11')
window.set_default_size(400, 300)
area = Gtk.DrawingArea()
window.add(area)
color = (0.1, 0.2, 0.6)
previous = ''


def draw(widget, context):
    context.set_source_rgb(*color)
    context.paint()


def update():
    global color, previous
    try:
        command = open(sys.argv[1]).read()
    except FileNotFoundError:
        return True
    if command != previous and command.startswith('root '):
        previous = command
        value = int(command.split()[1], 16)
        color = tuple(((value >> shift) & 255) / 255 for shift in (16, 8, 0))
        area.queue_draw()
    return True


area.connect('draw', draw)
window.connect('destroy', Gtk.main_quit)
GLib.timeout_add(20, update)
window.show_all()
Gtk.main()
