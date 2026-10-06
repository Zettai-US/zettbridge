// SPDX-License-Identifier: GPL-2.0-only
use crate::State;
impl State {
    pub(crate) fn fault(&mut self) {
        if self.state == 6 {
            return;
        }
        self.state = 6;
        self.frozen = true;
        self.revoke_pending = !self.provider && self.pool_size != 0;
        self.usage_valid = [false; 2];
        self.seq = self.seq.saturating_add(1);
        for l in &mut self.ledger.leases {
            if l.state != 0 {
                l.state = 3;
            }
        }
        // QuarantineOwner is this pinned, device-independent allocation: no
        // devres/Drop path can release backing or allow another attach.
        if self.backing.present() || self.pool_size != 0 {
            self.pin();
        }
    }
}
