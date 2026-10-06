# shellcheck shell=bash disable=SC2034  # PROFILE and RENDER are read by record.sh.
# A terminal's icon becomes that of the program running in it.

PROFILE=(2 18 12 14 6)
# Framed on the bar and the titlebar; ends back at the prompt.
RENDER=(0.6 9.2 1120 630 '1.55+0.08*min((on/30)/14,1)')

scene() {
    reset_desktop
    m 'exec alacritty'; wait_for Alacritty; sleep 1.5
    m 'workspace number 2'; m 'exec nautilus'; sleep 2
    m 'workspace number 1'; sleep 0.5
    typ clear; sleep 0.5

    rec_start terminal
    sleep 1.5
    typ 'nvim src/main.rs'; sleep 2.8
    keys -k Escape; typ ':q'; sleep 1.5
    typ 'nvim Cargo.toml'; sleep 2.5
    rec_stop
}
