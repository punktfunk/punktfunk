#!/bin/sh
# The seats package's removal step, never an upgrade's. With the door on, it goes off for the owner
# it is on for: the files go home, the seats stop, and the record and the copy mark go, so a
# reinstall copies the owner's files afresh. An owner whose account is gone leaves them in
# /var/lib/punktfunk. With no record, or a helper that can't hand the files home, the door's units
# are only disabled and the record stays for the next `off`. Always exits 0: a removal never fails
# here.
helper=/usr/libexec/punktfunk/door-helper
owner=$(cat /var/lib/punktfunk-seats/owner 2>/dev/null)
if [ -n "$owner" ] && [ -x "$helper" ] && "$helper" off "$owner"; then
    exit 0
fi
systemctl disable --now punktfunk-web-door.service punktfunk-door.service punktfunk-seats.service 2>/dev/null
exit 0
