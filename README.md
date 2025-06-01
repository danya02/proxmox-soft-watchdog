# proxmox-soft-watchdog
Service on the Proxmox host that watches over Proxmox guest VMs using guest-agent

## Installation inside guest

```bash
wget -O /etc/systemd/system/watchdog-feed.service https://raw.githubusercontent.com/danya02/proxmox-soft-watchdog/refs/heads/main/watchdog-feed.service
wget -O /etc/systemd/system/watchdog-feed.timer https://raw.githubusercontent.com/danya02/proxmox-soft-watchdog/refs/heads/main/watchdog-feed.timer

# If needed, update watchdog-feed.service

systemctl daemon-reload
systemctl enable --now watchdog-feed.timer
```