# shellcheck shell=bash disable=SC2034  # PROFILE and RENDER are read by record.sh.
# Apps open across three workspaces, some of them into stacks, tabs and nested
# splits, filling the bar and the container titles with icons.

# A 1280x720 desktop, framed whole: big enough titles to read the layouts.
PROFILE=(2.5 24 15 10 4)
# start duration width height zoom
RENDER=(1.1 19.8 1280 720 '1+0.03*min((on/30)/19.8,1)')

scene() {
    reset_desktop
    stage org.mozilla.firefox firefox --no-remote "${SITES[0]}"
    sleep 4
    stage org.gnome.Nautilus nautilus
    stage org.gnome.TextEditor gnome-text-editor
    stage org.gnome.SystemMonitor gnome-system-monitor
    stage org.gnome.Loupe loupe /usr/share/backgrounds/gnome/fold-d.jxl
    stage org.gnome.Calculator gnome-calculator
    m 'workspace number 9'; m 'workspace number 1'; sleep 1

    rec_start hero
    sleep 1.2
    # Workspace 1: the browser beside a stack holding a terminal and a vertical split.
    bring org.mozilla.firefox 1; sleep 1.2
    m 'exec alacritty'; wait_for Alacritty; sleep 0.6
    kb v; kb s; sleep 0.6
    bring org.gnome.Nautilus 1; sleep 0.8
    m '[app_id="org.gnome.Nautilus"] focus'; kb v
    bring org.gnome.TextEditor 1; sleep 1.6
    # Workspace 2: tabs, one of them two apps side by side.
    m 'workspace number 2'; sleep 0.4
    bring org.gnome.SystemMonitor 2; sleep 0.6
    kb w; bring org.gnome.Loupe 2; sleep 0.8
    m '[app_id="org.gnome.Loupe"] focus'; kb b
    bring org.gnome.Calculator 2; sleep 1.6
    # Workspace 3: three of the same app make one icon with a count.
    m 'workspace number 3'; sleep 0.3
    m 'exec alacritty'; sleep 0.5; m 'exec alacritty'; sleep 0.5; m 'exec alacritty'; sleep 1.2
    # A look around.
    m 'workspace number 1'; sleep 1.6
    m '[app_id="Alacritty"] focus'; sleep 1.2
    m 'workspace number 2'; sleep 1.4
    m '[app_id="org.gnome.SystemMonitor"] focus'; sleep 1.4
    m 'workspace number 1'; sleep 2
    rec_stop
}
