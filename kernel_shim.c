// SPDX-License-Identifier: GPL-2.0-only
/* Stateless helpers for macros/inlines absent from Rust bindings, and the
 * private DAX bus ABI. Device/file/work/backing ownership lives in Rust. */
#include "bus.h"
#include "dax-private.h"
#include <linux/dax.h>
#include <linux/dma-map-ops.h>
#include <linux/dma-mapping.h>
#include <linux/io.h>
#include <linux/iommu.h>
#include <linux/module.h>
#include <linux/nodemask.h>
#include <linux/pci.h>
#include <linux/scatterlist.h>

extern void *zb_rs_dax_probe(struct device *, u64, u64);
extern void zb_rs_dax_remove(void *);
extern void zb_rs_offline(struct pci_dev *);
extern ssize_t zb_rs_show(void *, int, int, char *);

/* Rust PCI Adapter installs these hooks before native registration. */
static pci_ers_result_t zb_error(struct pci_dev *p, pci_channel_state_t s) {
  zb_rs_offline(p);
  return PCI_ERS_RESULT_DISCONNECT;
}
static void zb_reset(struct pci_dev *p) { zb_rs_offline(p); }
static int zb_suspend(struct device *d) {
  zb_rs_offline(to_pci_dev(d));
  return -EBUSY;
}
static const struct pci_error_handlers zb_errors = {.error_detected = zb_error,
                                                    .reset_prepare = zb_reset};
static const struct dev_pm_ops zb_pm = {
    .suspend = zb_suspend, .freeze = zb_suspend, .poweroff = zb_suspend};
void zb_k_pci_hooks(struct pci_driver *d);
void zb_k_pci_hooks(struct pci_driver *d) {
  d->err_handler = &zb_errors;
  d->driver.pm = &zb_pm;
}
bool zb_k_dma_supported(struct device *d);
bool zb_k_dma_supported(struct device *d) {
  return device_iommu_mapped(d) && dev_is_dma_coherent(d);
}
void zb_k_max_segment(struct device *d);
void zb_k_max_segment(struct device *d) { dma_set_max_seg_size(d, SZ_2M); }
void zb_k_barrier(void);
void zb_k_barrier(void) { dma_wmb(); }
struct page *zb_k_alloc_page(int node);
struct page *zb_k_alloc_page(int node) {
  if (node < 0 || node >= MAX_NUMNODES || !node_online(node))
    return NULL;
  return alloc_pages_node(node, GFP_KERNEL | __GFP_ZERO | __GFP_THISNODE, 0);
}
int zb_k_page_node(struct page *p);
int zb_k_page_node(struct page *p) { return page_to_nid(p); }
u64 zb_k_page_phys(struct page *p);
u64 zb_k_page_phys(struct page *p) { return page_to_phys(p); }
void *zb_k_page_address(struct page *p);
void *zb_k_page_address(struct page *p) { return page_address(p); }
int zb_k_sg_alloc(struct sg_table *t, struct page **pages, unsigned int n);
int zb_k_sg_alloc(struct sg_table *t, struct page **pages, unsigned int n) {
  return sg_alloc_table_from_pages_segment(t, pages, n, 0, (u64)n * PAGE_SIZE,
                                           SZ_2M, GFP_KERNEL);
}
struct scatterlist *zb_k_sg_next(struct scatterlist *s);
struct scatterlist *zb_k_sg_next(struct scatterlist *s) { return sg_next(s); }
bool zb_k_sg_bounce(struct scatterlist *s);
bool zb_k_sg_bounce(struct scatterlist *s) { return sg_dma_is_swiotlb(s); }
void zb_k_sg_unmap(struct device *d, struct sg_table *t);
void zb_k_sg_unmap(struct device *d, struct sg_table *t) {
  dma_unmap_sgtable(d, t, DMA_BIDIRECTIONAL, 0);
}
void zb_k_sg_cpu(struct device *d, struct sg_table *t);
void zb_k_sg_cpu(struct device *d, struct sg_table *t) {
  dma_sync_sgtable_for_cpu(d, t, DMA_BIDIRECTIONAL);
}
void zb_k_sg_device(struct device *d, struct sg_table *t);
void zb_k_sg_device(struct device *d, struct sg_table *t) {
  dma_sync_sgtable_for_device(d, t, DMA_BIDIRECTIONAL);
}
void zb_k_vma_prepare(struct vm_area_struct *v,
                      const struct vm_operations_struct *ops);
void zb_k_vma_prepare(struct vm_area_struct *v,
                      const struct vm_operations_struct *ops) {
  vm_flags_set(v, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP);
  vm_flags_clear(v, VM_MAYEXEC);
  v->vm_page_prot = pgprot_noncached(v->vm_page_prot);
  v->vm_ops = ops;
}
bool zb_k_claim(u64 base, u64 size);
bool zb_k_claim(u64 base, u64 size) {
  return request_mem_region(base, size, "zettbridge-managed-pool") != NULL;
}
void zb_k_unclaim(u64 base, u64 size);
void zb_k_unclaim(u64 base, u64 size) { release_mem_region(base, size); }

/* DAX's bus types are private and are not in generated Rust bindings. */
static int zb_dax_probe(struct dev_dax *d) {
  void *owner;
  if (d->nr_range != 1 || d->ranges[0].pgoff)
    return -ENODEV;
  owner = zb_rs_dax_probe(&d->dev, d->ranges[0].range.start,
                          range_len(&d->ranges[0].range));
  if (IS_ERR(owner))
    return PTR_ERR(owner);
  dev_set_drvdata(&d->dev, owner);
  return 0;
}
static void zb_dax_remove(struct dev_dax *d) {
  zb_rs_dax_remove(dev_get_drvdata(&d->dev));
}
static struct dax_device_driver zb_dax_driver = {
    .drv = {.name = "zettbridge", .suppress_bind_attrs = true},
    .type = DAXDRV_DEVICE_TYPE,
    .probe = zb_dax_probe,
    .remove = zb_dax_remove,
};

/* Sysfs attribute declarations only. Rust locks, snapshots and formats values.
 */
static struct class *zb_class;
struct zb_attr {
  struct device_attribute attr;
  int tier, field;
};
static ssize_t zb_show(struct device *d, struct device_attribute *a,
                       char *buf) {
  struct zb_attr *za = container_of(a, struct zb_attr, attr);
  return zb_rs_show(dev_get_drvdata(d), za->tier, za->field, buf);
}
#define A(n, t, f)                                                             \
  static struct zb_attr a_##n = {__ATTR(n, 0444, zb_show, NULL), t, f}
A(state, -1, 0);
A(epoch, -1, 1);
A(stats_seq, -1, 2);
A(allocatable, -1, 3);
A(usage_valid, -1, 4);
A(usage_age_ns, -1, 5);
A(ats_status, -1, 6);
A(ats_requests, -1, 7);
A(ats_hits, -1, 8);
A(ats_invalidations, -1, 9);
A(ats_cached_entries, -1, 10);
A(ats_flushes, -1, 11);
A(ats_faults, -1, 12);
A(ats_transitions, -1, 13);
A(ats_enabled, -1, 14);
#define T(n, f)                                                                \
  static struct zb_attr dram_##n = {__ATTR(n, 0444, zb_show, NULL), 0, f};     \
  static struct zb_attr cxl_##n = {__ATTR(n, 0444, zb_show, NULL), 1, f}
T(installed_bytes, 0);
T(reserved_bytes, 1);
T(user_pinned_bytes, 2);
T(dma_mapped_bytes, 3);
T(exported_bytes, 4);
T(allocator_capacity_bytes, 5);
T(allocated_bytes, 6);
T(releasing_bytes, 7);
T(quarantined_bytes, 8);
T(available_bytes, 9);
T(requested_bytes, 10);
T(cache_resident_bytes, 11);
T(quarantined_backing_bytes, 12);
static struct attribute *root_attrs[] = {&a_state.attr.attr,
                                         &a_epoch.attr.attr,
                                         &a_stats_seq.attr.attr,
                                         &a_allocatable.attr.attr,
                                         &a_usage_valid.attr.attr,
                                         &a_usage_age_ns.attr.attr,
                                         &a_ats_status.attr.attr,
                                         &a_ats_requests.attr.attr,
                                         &a_ats_hits.attr.attr,
                                         &a_ats_invalidations.attr.attr,
                                         &a_ats_cached_entries.attr.attr,
                                         &a_ats_flushes.attr.attr,
                                         &a_ats_faults.attr.attr,
                                         &a_ats_transitions.attr.attr,
                                         &a_ats_enabled.attr.attr,
                                         NULL};
#define TA(t)                                                                  \
  &t##_installed_bytes.attr.attr, &t##_reserved_bytes.attr.attr,               \
      &t##_user_pinned_bytes.attr.attr, &t##_dma_mapped_bytes.attr.attr,       \
      &t##_exported_bytes.attr.attr, &t##_allocator_capacity_bytes.attr.attr,  \
      &t##_allocated_bytes.attr.attr, &t##_releasing_bytes.attr.attr,          \
      &t##_quarantined_bytes.attr.attr, &t##_available_bytes.attr.attr,        \
      &t##_requested_bytes.attr.attr, &t##_cache_resident_bytes.attr.attr,     \
      &t##_quarantined_backing_bytes.attr.attr, NULL
static struct attribute *dram_attrs[] = {TA(dram)}, *cxl_attrs[] = {TA(cxl)};
static const struct attribute_group root_group = {.attrs = root_attrs};
static const struct attribute_group dram_group = {.name = "dram",
                                                  .attrs = dram_attrs};
static const struct attribute_group cxl_group = {.name = "cxl",
                                                 .attrs = cxl_attrs};
static const struct attribute_group *zb_groups[] = {&root_group, &dram_group,
                                                    &cxl_group, NULL};

int zb_k_platform_register(void);
int zb_k_platform_register(void) {
  int ret;
  zb_class = class_create("rpu");
  if (IS_ERR(zb_class))
    return PTR_ERR(zb_class);
  ret = dax_driver_register(&zb_dax_driver);
  if (ret)
    class_destroy(zb_class);
  return ret;
}
void zb_k_platform_unregister(void);
void zb_k_platform_unregister(void) {
  dax_driver_unregister(&zb_dax_driver);
  class_destroy(zb_class);
}
struct device *zb_k_class_add(struct device *parent, void *owner,
                              bool provider);
struct device *zb_k_class_add(struct device *parent, void *owner,
                              bool provider) {
  return device_create_with_groups(zb_class, parent, 0, owner, zb_groups,
                                   provider ? "rpu0" : "consumer0");
}
