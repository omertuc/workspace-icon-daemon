# shellcheck shell=bash disable=SC2034  # PROFILE and RENDER are read by record.sh.
# The browser's icon follows the site it shows.

PROFILE=(2 18 12 14 6)
# Framed on the bar, the titlebar and the address bar.
RENDER=(0.6 11.4 1120 630 '1.55+0.08*min((on/30)/14,1)')

scene() {
    reset_desktop
    m "exec firefox --no-remote ${SITES[0]}"; wait_for org.mozilla.firefox; sleep 5
    m 'workspace number 2'; m 'exec alacritty'; sleep 1.5
    m 'workspace number 1'; sleep 1

    rec_start browser
    sleep 1.5
    for site in "${SITES[@]:1}"; do
        keys -M ctrl -k l -m ctrl; sleep 0.3
        typ "${site#https://}"; sleep 2.8
    done
    rec_stop
}
