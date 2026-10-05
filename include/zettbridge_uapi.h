/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef ZETTBRIDGE_UAPI_H
#define ZETTBRIDGE_UAPI_H
#include <linux/ioctl.h>
#include <linux/types.h>
struct zb_request {
    __u32 struct_size, flags;
    __u64 request_id, export_epoch, object_handle;
    __u64 bytes, alignment;
    __u32 tier;
    __s32 numa_node;
    __u32 status, reserved;
};
struct zb_stats {
    __u32 struct_size, source_flags, provider_state, tier;
    __u64 epoch, stats_seq, capture_mono_ns;
    __u64 session_lo, session_hi, report_seq, usage_age_ns, valid_mask;
    __u64 installed, reserved_bytes, user_pinned, dma_mapped, exported;
    __u64 allocator_capacity, allocated, releasing, quarantined, available;
    __u64 requested, cache_resident, quarantined_backing;
    __u8 reserved[72];
};
struct zb_caps {
    __u32 struct_size, major, minor, role;
    __u64 features, min_alignment, max_extents, max_bytes;
    __u64 reserved[2];
};
#define ZB_IOC_CAPS _IOR('Z', 0, struct zb_caps)
#define ZB_IOC_STATS _IOWR('Z', 1, struct zb_stats)
#define ZB_IOC_RESERVE _IOWR('Z', 2, struct zb_request)
#define ZB_IOC_PREPARE _IOWR('Z', 3, struct zb_request)
#define ZB_IOC_COMMIT _IOWR('Z', 4, struct zb_request)
#define ZB_IOC_QUIESCE _IOWR('Z', 5, struct zb_request)
#define ZB_IOC_RECLAIM _IOWR('Z', 6, struct zb_request)
#define ZB_IOC_ALLOC _IOWR('Z', 7, struct zb_request)
#define ZB_IOC_RELEASE _IOWR('Z', 8, struct zb_request)
#define ZB_IOC_QUERY_TXN _IOWR('Z', 9, struct zb_request)
/* status: 0 completed, 1 pending (poll stats / retry original request_id). */
#endif
