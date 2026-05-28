#!/usr/bin/env python3
import gi
import pathlib

gi.require_version('Gtk', '3.0')
gi.require_version('Gdk', '3.0')
from gi.repository import Gtk, Gdk, GLib, Pango

STATUS = pathlib.Path('/tmp/dictate-status')
REC = pathlib.Path('/tmp/dictate-recording')

class StatusOSD(Gtk.Window):
    def __init__(self):
        super().__init__(type=Gtk.WindowType.POPUP)
        self.set_decorated(False)
        self.set_keep_above(True)
        self.set_skip_taskbar_hint(True)
        self.set_skip_pager_hint(True)
        self.set_app_paintable(True)
        self.set_type_hint(Gdk.WindowTypeHint.NOTIFICATION)
        self.set_accept_focus(False)

        self.label = Gtk.Label()
        self.label.set_use_markup(True)
        self.label.set_margin_top(8)
        self.label.set_margin_bottom(8)
        self.label.set_margin_start(12)
        self.label.set_margin_end(12)
        self.add(self.label)

        css = b"""
        window { background: rgba(0,0,0,0.78); border-radius: 12px; }
        label { color: white; font: 700 15px Sans; }
        """
        provider = Gtk.CssProvider()
        provider.load_from_data(css)
        Gtk.StyleContext.add_provider_for_screen(
            Gdk.Screen.get_default(), provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
        )

        GLib.timeout_add(200, self.tick)
        self.tick()

    def current_status(self):
        if REC.exists():
            try:
                mode = REC.read_text().strip().lower()
            except Exception:
                mode = ''
            if mode == 'agent':
                return '<span foreground="#bb66ff">◆ AGENT REC</span>'
            return '<span foreground="#ff3333">● DICTATE REC</span>'
        try:
            text = STATUS.read_text().strip()
        except Exception:
            text = ''
        if not text:
            return ''
        escaped = GLib.markup_escape_text(text)
        return f'<span foreground="#ffaa33">{escaped}</span>'

    def tick(self):
        text = self.current_status()
        if text:
            self.label.set_markup(text)
            self.show_all()
            self.position_top_right()
        else:
            self.hide()
        return True

    def position_top_right(self):
        self.resize(1, 1)
        self.get_window().process_updates(True) if self.get_window() else None
        screen = Gdk.Screen.get_default()
        monitor = screen.get_primary_monitor()
        geo = screen.get_monitor_geometry(monitor)
        width, height = self.get_size()
        self.move(geo.x + geo.width - width - 18, geo.y + 36)

win = StatusOSD()
Gtk.main()
