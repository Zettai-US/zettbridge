/* SPDX-License-Identifier: GPL-2.0-only */
#define _GNU_SOURCE
#include "zettbridge_uapi.h"
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <pthread.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
_Static_assert(sizeof(struct zb_request) == 64, "request ABI");
_Static_assert(sizeof(struct zb_stats) == 256, "stats ABI");
static uint64_t pool_bytes;
static bool live;
static bool ats_required;
static bool ats_test_quarantine;
static void die(const char *s);

static uint64_t ats_field(const char *field) {
  char path[160];
  snprintf(path, sizeof(path), "/sys/class/rpu/rpu0/ats_%s", field);
  FILE *file = fopen(path, "r");
  if (!file)
    die("ATS sysfs");
  uint64_t value;
  if (fscanf(file, "%" SCNu64, &value) != 1) {
    fclose(file);
    die("ATS counter");
  }
  fclose(file);
  return value;
}

static void die(const char *s) {
  perror(s);
  exit(1);
}
static void check(bool b, const char *s) {
  if (!b) {
    fprintf(stderr, "FAIL %s\n", s);
    exit(1);
  }
}
static void verify_ats(bool retired) {
  if (!ats_required)
    return;
  for (unsigned i = 0; i < 30; i++) {
    if (ats_field("hits") && ats_field("invalidations") >= 2 &&
        (!retired || !ats_field("cached_entries")))
      break;
    usleep(100000);
  }
  check(ats_field("status") == 7 && ats_field("enabled") == 1,
        "provider ATS remains enabled with invalidation notifier");
  check(ats_field("transitions") == 1, "ATS never toggled during mapping lifetime");
  check(ats_field("requests") && ats_field("hits") &&
            ats_field("invalidations") >= 2 && !ats_field("faults"),
        "ATS translation, cache hit and Linux invalidation counters");
  check(!retired || !ats_field("cached_entries"), "retire leaves no cached translation");
  printf("ATS_CONTINUOUS_PASS status=%" PRIu64 " requests=%" PRIu64
         " hits=%" PRIu64 " invalidations=%" PRIu64 " entries=%" PRIu64
         " transitions=%" PRIu64 " retired=%d\n", ats_field("status"),
         ats_field("requests"), ats_field("hits"), ats_field("invalidations"),
         ats_field("cached_entries"), ats_field("transitions"), retired);
  fflush(stdout);
}
static void call(int fd, unsigned long op, void *r) {
  if (ioctl(fd, op, r))
    die("ioctl");
}
static struct zb_stats stats(int fd) {
  struct zb_stats s = {.struct_size = 256};
  call(fd, ZB_IOC_STATS, &s);
  return s;
}
static struct zb_request req(uint64_t id, uint64_t epoch, uint64_t handle) {
  return (struct zb_request){.struct_size = 64,
                             .request_id = id,
                             .export_epoch = epoch,
                             .object_handle = handle};
}
static void show(struct zb_stats s) {
  printf("state=%u epoch=%llu reserved=%llu mapped=%llu exported=%llu "
         "allocated=%llu releasing=%llu quarantined=%llu available=%llu "
         "valid=0x%llx flags=0x%x report=%llu\n",
         s.provider_state, s.epoch, s.reserved_bytes, s.dma_mapped, s.exported,
         s.allocated, s.releasing, s.quarantined, s.available, s.valid_mask,
         s.source_flags, s.report_seq);
  fflush(stdout);
}
static void wait_state(int fd, unsigned state) {
  for (int i = 0; i < 6000; i++) {
    struct zb_stats s = stats(fd);
    if (s.provider_state == state)
      return;
    if (s.provider_state == 6) {
      show(s);
      fprintf(stderr, "faulted\n");
      exit(1);
    }
    usleep(100000);
  }
  die("state timeout");
}
static void provider(int fd, bool quarantine) {
  struct zb_request r = req(1, 0, 0);
  r.bytes = pool_bytes;
  r.numa_node = 0;
  puts("PROVIDER RESERVE_START");
  fflush(stdout);
  call(fd, ZB_IOC_RESERVE, &r);
  puts("PROVIDER RESERVED");
  fflush(stdout);
  check(r.object_handle == 1, "reserve handle");
  r = req(2, 0, 1);
  puts("PROVIDER PREPARE_START");
  fflush(stdout);
  if (ats_test_quarantine) {
    check(ioctl(fd, ZB_IOC_PREPARE, &r) < 0 && errno == ETIMEDOUT,
          "ambiguous ATS test timeout");
    struct zb_stats s = stats(fd);
    show(s);
    check(s.provider_state == 6 && s.reserved_bytes == pool_bytes &&
              !s.dma_mapped && s.quarantined_backing == pool_bytes,
          "ATS test retains backing and temporary coherent buffer");
    r = req(4, s.epoch, 1);
    check(ioctl(fd, ZB_IOC_RECLAIM, &r) < 0 && errno == EBUSY,
          "ATS uncertain completion denies reclaim");
    usleep(200000);
    check(ats_field("enabled") && ats_field("cached_entries") == 1 &&
              !ats_field("invalidations"),
          "temporary ATS mapping survives ambiguous completion");
    puts("PROVIDER QUARANTINE_PASS ATS temporary buffer retained");
    return;
  }
  call(fd, ZB_IOC_PREPARE, &r);
  puts("PROVIDER DMA_FULL_RANGE_PASS");
  verify_ats(false);
  fflush(stdout);
  r = req(3, 0, 1);
  if (quarantine) {
    check(ioctl(fd, ZB_IOC_COMMIT, &r) < 0 && errno == ETIMEDOUT,
          "ambiguous commit timeout");
    struct zb_stats s = stats(fd);
    show(s);
    check(s.provider_state == 6 && s.reserved_bytes == pool_bytes &&
              s.dma_mapped == pool_bytes &&
              s.quarantined_backing == pool_bytes && !(s.source_flags & 16) &&
              !(s.valid_mask & (1UL << 4)),
          "backing quarantine");
    r = req(4, s.epoch, 1);
    check(ioctl(fd, ZB_IOC_RECLAIM, &r) < 0 && errno == EBUSY,
          "unsafe reclaim denied");
    puts("PROVIDER QUARANTINE_PASS");
    return;
  }
  call(fd, ZB_IOC_COMMIT, &r);
  uint64_t epoch = r.export_epoch;
  puts("PROVIDER COMMITTED");
  fflush(stdout);
  wait_state(fd, 3);
  if (live) {
    printf("PROVIDER READY capacity=%" PRIu64 "\n", pool_bytes);
    fflush(stdout);
    while (access("/run/zettbridge-stop", F_OK) != 0) {
      struct zb_stats s = stats(fd);
      check(s.provider_state == 3, "live provider remains active");
      show(s);
      verify_ats(false);
      sleep(5);
    }
  } else {
    bool saw = false;
    for (int i = 0; i < 6000; i++) {
      struct zb_stats s = stats(fd);
      if ((s.source_flags & 2) && s.allocated) {
        saw = true;
        show(s);
      }
      if (saw && (s.source_flags & 2) && !s.allocated && !s.releasing &&
          !s.quarantined) {
        sleep(2);
        s = stats(fd);
        if ((s.source_flags & 2) && !s.allocated && !s.releasing &&
            !s.quarantined)
          break;
      }
      if (i == 5999)
        die("usage mirror timeout");
      usleep(100000);
    }
  }
  r = req(4, epoch, 1);
  call(fd, ZB_IOC_QUIESCE, &r);
  wait_state(fd, 5);
  r = req(5, epoch, 1);
  call(fd, ZB_IOC_RECLAIM, &r);
  struct zb_stats s = stats(fd);
  show(s);
  check(!s.reserved_bytes && !s.dma_mapped && !s.exported,
        "provider reclaimed");
  verify_ats(true);
  puts(live ? "PROVIDER STOPPED" : "PROVIDER PASS");
}
struct stress_arg {
  int fd;
  uint64_t epoch;
  unsigned thread;
};
static void *stress(void *arg) {
  struct stress_arg *a = arg;
  for (unsigned i = 0; i < 8; i++) {
    uint64_t id = 100 + a->thread * 32 + i * 2;
    struct zb_request r = req(id, a->epoch, 0);
    r.bytes = 1 + i;
    r.alignment = 4096;
    call(a->fd, ZB_IOC_ALLOC, &r);
    uint64_t handle = r.object_handle;
    struct zb_stats s = stats(a->fd);
    check(s.requested <= s.allocated &&
              s.allocated + s.releasing + s.quarantined + s.available ==
                  s.allocator_capacity,
          "concurrent snapshot accounting");
    r = req(id + 1, a->epoch, handle);
    call(a->fd, ZB_IOC_RELEASE, &r);
  }
  return NULL;
}
/* Cover the entire advertised DPA range, including >4-GiB addresses and the
 * last eight bytes. All windows stay mapped until the verification pass, so
 * truncation/aliasing between windows cannot accidentally pass. */
static void span_probe(int fd, uint64_t epoch) {
  struct window {
    volatile uint8_t *p;
    uint64_t handle, base, size;
  } windows[64];
  unsigned count = 0;
  uint64_t base = 0;
  while (base < pool_bytes) {
    check(count < 64, "span window limit");
    uint64_t bytes = pool_bytes - base;
    if (bytes > (1UL << 30))
      bytes = 1UL << 30;
    struct zb_request r = req(1000 + count * 2, epoch, 0);
    r.bytes = bytes;
    r.alignment = 4096;
    call(fd, ZB_IOC_ALLOC, &r);
    volatile uint8_t *p = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED,
                               fd, (off_t)r.object_handle << 32);
    check(p != MAP_FAILED, "span mmap");
    windows[count] = (struct window){p, r.object_handle, base, bytes};
    for (uint64_t off = 0; off < bytes; off += 1UL << 20) {
      volatile uint64_t *word = (volatile uint64_t *)(p + off);
      check(*word == 0, "span initially zero");
      *word = 0x5a77c0de12345678ULL ^ (base + off);
    }
    volatile uint64_t *last = (volatile uint64_t *)(p + bytes - 8);
    check(*last == 0, "aperture tail initially zero");
    *last = 0x5a77c0de12345678ULL ^ (base + bytes - 8);
    printf("CONSUMER SPAN_WINDOW base=%" PRIu64 " bytes=%" PRIu64 "\n", base,
           bytes);
    fflush(stdout);
    base += bytes;
    count++;
  }
  check(stats(fd).allocated == pool_bytes, "entire pool leased");
  for (unsigned i = 0; i < count; i++) {
    struct window *w = &windows[i];
    for (uint64_t off = 0; off < w->size; off += 1UL << 20)
      check(*(volatile uint64_t *)(w->p + off) ==
                (0x5a77c0de12345678ULL ^ (w->base + off)),
            "span readback without alias");
    check(*(volatile uint64_t *)(w->p + w->size - 8) ==
              (0x5a77c0de12345678ULL ^ (w->base + w->size - 8)),
          "aperture tail readback");
  }
  printf("CONSUMER APERTURE_SPAN_PASS bytes=%" PRIu64 " last_dpa=%" PRIu64 "\n",
         pool_bytes, pool_bytes - 1);
  fflush(stdout);
  for (unsigned i = 0; i < count; i++) {
    munmap((void *)windows[i].p, windows[i].size);
    struct zb_request r = req(1001 + i * 2, epoch, windows[i].handle);
    call(fd, ZB_IOC_RELEASE, &r);
  }
  for (int i = 0; i < 6000; i++) {
    struct zb_stats s = stats(fd);
    if (!s.allocated && !s.releasing)
      return;
    check(s.provider_state != 6, "span release not faulted");
    usleep(100000);
  }
  check(false, "span release completed");
}
static void consumer_live(int fd, uint64_t epoch) {
  struct zb_request r = req(900, epoch, 0);
  r.bytes = 128UL << 20;
  r.alignment = 4096;
  call(fd, ZB_IOC_ALLOC, &r);
  uint64_t handle = r.object_handle, bytes = r.bytes;
  volatile uint64_t *p = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED,
                              fd, (off_t)handle << 32);
  check(p != MAP_FAILED, "live mmap");
  printf("CONSUMER READY capacity=%" PRIu64 " lease=%" PRIu64 " handle=%" PRIu64
         "\n",
         pool_bytes, bytes, handle);
  fflush(stdout);
  uint64_t seq = 1;
  while (access("/run/zettbridge-stop", F_OK) != 0) {
    p[0] = seq;
    p[bytes / 8 - 1] = ~seq;
    check(p[0] == seq && p[bytes / 8 - 1] == ~seq, "live CXL heartbeat");
    struct zb_stats s = stats(fd);
    check(s.provider_state == 3 && s.allocator_capacity == pool_bytes,
          "live pool active");
    printf("CONSUMER HEARTBEAT seq=%" PRIu64 "\n", seq++);
    show(s);
    sleep(5);
  }
  munmap((void *)p, bytes);
  r = req(901, epoch, handle);
  call(fd, ZB_IOC_RELEASE, &r);
  for (int i = 0; stats(fd).releasing && i < 6000; i++)
    usleep(100000);
  check(!stats(fd).releasing, "live lease released");
  int done = open("/run/zettbridge-consumer-drained", O_WRONLY | O_CREAT, 0600);
  check(done >= 0, "mark drained");
  close(done);
  wait_state(fd, 5);
  puts("CONSUMER STOPPED");
}
static void consumer(int fd) {
  wait_state(fd, 3);
  uint64_t epoch = stats(fd).epoch;
  check(stats(fd).allocator_capacity == pool_bytes, "configured pool capacity");
  if (pool_bytes > (256UL << 20) || live)
    span_probe(fd, epoch);
  if (live) {
    consumer_live(fd, epoch);
    return;
  }
  struct zb_request r = req(1, epoch, 0);
  r.bytes = 8193;
  r.alignment = 4096;
  struct zb_request original = r;
  struct zb_request *readonly = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  check(readonly != MAP_FAILED, "copyout test buffer");
  *readonly = r;
  check(mprotect(readonly, 4096, PROT_READ) == 0, "copyout protect");
  check(ioctl(fd, ZB_IOC_ALLOC, readonly) < 0 && errno == EFAULT,
        "failed copyout");
  munmap(readonly, 4096);
  // Retrying identical owner/request_id retrieves the already allocated lease.
  call(fd, ZB_IOC_ALLOC, &r);
  uint64_t handle = r.object_handle, bytes = r.bytes;
  check(bytes == 12288, "rounding");
  struct zb_request retry = original;
  call(fd, ZB_IOC_ALLOC, &retry);
  check(retry.object_handle == handle, "dedup");
  retry = req(1, 0, 0);
  call(fd, ZB_IOC_QUERY_TXN, &retry);
  check(retry.object_handle == handle, "query transaction");
  retry = original;
  retry.flags = 1;
  check(ioctl(fd, ZB_IOC_ALLOC, &retry) < 0 && errno == EINVAL, "flags");
  int other = open("/dev/rpu_consumer0", O_RDWR);
  check(other >= 0, "second owner");
  retry = req(1, epoch, handle);
  check(ioctl(other, ZB_IOC_RELEASE, &retry) < 0 && errno == ENOENT,
        "owner isolation");
  close(other);
  volatile uint8_t *p = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED,
                             fd, (off_t)handle << 32);
  if (p == MAP_FAILED)
    die("mmap");
  for (uint64_t i = 0; i < bytes; i++) {
    check(p[i] == 0, "zero before mapping");
    p[i] = (uint8_t)(i ^ 0xa5);
  }
  for (uint64_t i = 0; i < bytes; i++)
    check(p[i] == (uint8_t)(i ^ 0xa5), "remote CXL load/store");
  pthread_t threads[4];
  struct stress_arg args[4];
  for (unsigned i = 0; i < 4; i++) {
    args[i] = (struct stress_arg){fd, epoch, i};
    check(pthread_create(&threads[i], NULL, stress, &args[i]) == 0,
          "thread create");
  }
  for (unsigned i = 0; i < 4; i++)
    pthread_join(threads[i], NULL);
  for (int i = 0; i < 100 && stats(fd).releasing; i++)
    usleep(100000);
  puts("CONSUMER CONCURRENT_LEDGER_PASS");
  puts("CONSUMER CXL_DATA_PASS");
  show(stats(fd));
  sleep(2);
  int syncfd[2];
  if (pipe(syncfd))
    die("pipe");
  pid_t pid = fork();
  if (pid < 0)
    die("fork");
  if (!pid) {
    char c;
    close(syncfd[1]);
    if (read(syncfd[0], &c, 1) != 1)
      _exit(2);
    volatile uint8_t v = p[0];
    (void)v;
    _exit(3);
  }
  close(syncfd[0]);
  r = req(2, epoch, handle);
  call(fd, ZB_IOC_RELEASE, &r);
  check(r.status == 1, "release pending");
  for (int i = 0; i < 100; i++) {
    if (stats(fd).releasing == 0)
      break;
    usleep(100000);
  }
  check(stats(fd).allocated == 0 && stats(fd).releasing == 0,
        "released after barrier");
  check(write(syncfd[1], "x", 1) == 1, "wake child");
  close(syncfd[1]);
  int status;
  waitpid(pid, &status, 0);
  check(WIFSIGNALED(status) && WTERMSIG(status) == SIGBUS,
        "fork mapping revoked");
  munmap((void *)p, bytes);
  r = req(3, epoch, 0);
  bytes = 3UL << 20;
  r.bytes = bytes;
  r.alignment = 4096;
  call(fd, ZB_IOC_ALLOC, &r);
  p = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
           (off_t)r.object_handle << 32);
  if (p == MAP_FAILED)
    die("remap");
  for (uint64_t i = 0; i < 12288; i++)
    check(p[i] == 0, "zero on reuse");
  for (uint64_t i = 12288; i < bytes; i += 4096)
    check(p[i] == 0, "zero across SG segments");
  // DMA segments are capped at 2 MiB; exercise a request across that boundary.
  uint64_t pattern = 0x123456789abcdef0ULL, observed = 0;
  memcpy((void *)(p + (2UL << 20) - 4), &pattern, sizeof(pattern));
  memcpy(&observed, (const void *)(p + (2UL << 20) - 4), sizeof(observed));
  check(observed == pattern, "cross-segment read/write");
  // Split a VMA; the encoded handle and in-lease page offset must survive.
  check(mprotect((void *)(p + 4096), 4096, PROT_READ) == 0, "VMA split");
  check(p[4096] == 0 && p[8192] == 0, "split VMA offset");
  puts("CONSUMER SG_BOUNDARY_VMA_SPLIT_PASS");
  munmap((void *)p, bytes);
  uint64_t next = r.object_handle;
  r = req(4, epoch, next);
  call(fd, ZB_IOC_RELEASE, &r);
  puts("CONSUMER REVOKE_FORK_ZERO_PASS");
  fflush(stdout);
  wait_state(fd, 5);
  puts("CONSUMER PASS");
}
/* Exercise code/data lifetimes independently of publication. A live file must
 * pin the module, survive PCI unbind and fail closed before its final close. */
static void lifetime(int fd, bool prov) {
  char path[128], text[64] = {0}, bdf[32] = {0};
  snprintf(path, sizeof(path), "/sys/class/rpu/%s/state",
           prov ? "rpu0" : "consumer0");
  int attr = open(path, O_RDONLY);
  check(attr >= 0, "open Rust sysfs state");
  check(read(attr, text, sizeof(text) - 1) > 0 && !strcmp(text, "offline\n"),
        "Rust sysfs snapshot");
  close(attr);
  check(syscall(SYS_delete_module, "zettbridge", O_NONBLOCK) < 0 &&
            (errno == EWOULDBLOCK || errno == EBUSY),
        "open FD pins module");
  DIR *dir = opendir("/sys/bus/pci/drivers/zettbridge");
  check(dir != NULL, "PCI driver registered by Rust");
  struct dirent *entry;
  while ((entry = readdir(dir))) {
    if (strlen(entry->d_name) == 12 && entry->d_name[4] == ':' &&
        entry->d_name[10] == '.') {
      memcpy(bdf, entry->d_name, 13);
      break;
    }
  }
  closedir(dir);
  check(bdf[0] != 0, "bound management PCI function");
  int unbind = open("/sys/bus/pci/drivers/zettbridge/unbind", O_WRONLY);
  check(unbind >= 0 && write(unbind, bdf, strlen(bdf)) == (ssize_t)strlen(bdf),
        "unbind with live FD");
  close(unbind);
  struct zb_caps caps = {0};
  check(ioctl(fd, ZB_IOC_CAPS, &caps) < 0 && errno == ENODEV,
        "offline FD fails closed");
  check(syscall(SYS_delete_module, "zettbridge", O_NONBLOCK) < 0 &&
            (errno == EWOULDBLOCK || errno == EBUSY),
        "unbound FD still pins module");
  close(fd);
  check(syscall(SYS_delete_module, "zettbridge", O_NONBLOCK) == 0,
        "unload after final close");
  puts("RUST FD_UNBIND_UNLOAD_PASS");
}
int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(
        stderr,
        "usage: zbctl "
        "provider-test|consumer-test|provider-serve|consumer-serve|\n"
        "provider-lifetime|consumer-lifetime|provider-stats|consumer-stats\n");
    return 2;
  }
  live = strstr(argv[1], "serve") != NULL;
  ats_test_quarantine = strstr(argv[1], "ats-quarantine") != NULL;
  FILE *cmdline = fopen("/proc/cmdline", "r");
  char boot_args[4096];
  if (cmdline) {
    if (fgets(boot_args, sizeof(boot_args), cmdline))
      ats_required = strstr(boot_args, "zb.ats=on") != NULL;
    fclose(cmdline);
  }
  bool prov = !strncmp(argv[1], "provider", 8);
  int fd = open(prov ? "/dev/rpu_provider0" : "/dev/rpu_consumer0", O_RDWR);
  if (fd < 0)
    die("open");
  struct zb_caps caps = {0};
  call(fd, ZB_IOC_CAPS, &caps);
  check(caps.struct_size == 64 && caps.minor == 2, "caps");
  pool_bytes = caps.max_bytes;
  if (strstr(argv[1], "lifetime")) {
    lifetime(fd, prov);
    return 0;
  }
  if (strstr(argv[1], "stats"))
    show(stats(fd));
  else if (prov)
    provider(fd, strstr(argv[1], "quarantine") != NULL);
  else
    consumer(fd);
  close(fd);
  return 0;
}
