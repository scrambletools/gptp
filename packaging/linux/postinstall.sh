#!/bin/sh
# Makes the gptpd group and has systemd read the unit. It starts and
# enables nothing: triib and plaa start gptpd@INTERFACE as they need it.
if command -v systemd-sysusers >/dev/null 2>&1; then
    systemd-sysusers gptpd.conf || true
fi
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi
exit 0
