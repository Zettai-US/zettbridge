/* SPDX-License-Identifier: GPL-2.0-only */
#include <stddef.h>
#include "zettbridge_uapi.h"
_Static_assert(sizeof(struct zb_request) == 64, "request");
_Static_assert(sizeof(struct zb_stats) == 256, "stats");
_Static_assert(sizeof(struct zb_caps) == 64, "caps");
_Static_assert(offsetof(struct zb_request, request_id) == 8, "request ID");
_Static_assert(offsetof(struct zb_stats, installed) == 0x50, "installed");
_Static_assert(offsetof(struct zb_stats, quarantined_backing) == 0xb0, "quarantine");
_Static_assert(offsetof(struct zb_stats, reserved) == 0xb8, "reserved");
_Static_assert(ZB_IOC_STATS == 0xc1005a01U, "compat ioctl");
_Static_assert(ZB_IOC_ALLOC == 0xc0405a07U, "compat allocation");
