#!/usr/bin/env python3
"""Build an isolated test initramfs from the pinned kernel and this module."""
import argparse, os, pathlib, re, shutil, subprocess, tempfile
p=argparse.ArgumentParser();p.add_argument('--kernel',default='/root/linux');p.add_argument('--output',required=True)
a=p.parse_args();kernel=pathlib.Path(a.kernel);repo=pathlib.Path(__file__).resolve().parents[1]
def copy(root,src,dest=None):
 src=pathlib.Path(src);dest=root/str(dest or src).lstrip('/');dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(src,dest);shutil.copymode(src,dest)
with tempfile.TemporaryDirectory(prefix='zettbridge-initrd-') as tmp:
 root=pathlib.Path(tmp)
 for d in ['bin','sbin','dev','proc','sys','tmp','run','modules']: (root/d).mkdir(parents=True,exist_ok=True)
 copy(root,'/usr/bin/busybox','bin/busybox')
 for app in ['sh','mount','mkdir','sleep','cat','echo','insmod','rmmod','poweroff','dmesg','ls','grep','awk','sed','uname','touch','ps','kill','free']:(root/'bin'/app).symlink_to('busybox')
 copy(root,repo/'tools/zbctl','bin/zbctl');copy(root,repo/'zettbridge.ko','modules/zettbridge.ko')
 copy(root,'/usr/bin/cxl','bin/cxl')
 for lib in re.findall(r'(?:=>\s*)?(/[^\s]+)',subprocess.check_output(['ldd','/usr/bin/cxl'],text=True)):
  copy(root,lib)
 modules={f.stem.replace('-','_'):f for f in kernel.rglob('*.ko')}
 seen=set();order=[]
 def mod(name):
  if name in seen:return
  seen.add(name)
  if name not in modules:raise RuntimeError('missing kernel module '+name)
  path=modules[name]
  deps=subprocess.check_output(['modinfo','-F','depends',str(path)],text=True).strip()
  for dep in filter(None,deps.split(',')):mod(dep)
  copy(root,path,'modules/'+name+'.ko');order.append(name)
 for name in ['cxl_core','cxl_pci','cxl_acpi','cxl_port','cxl_mem','dax_cxl']:mod(name)
 (root/'modules/order').write_text('\n'.join(order)+'\n')
 init='''#!/bin/sh
export PATH=/bin:/sbin
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /tmp
role=consumer
mode=test
capacity=268435456
for x in $(cat /proc/cmdline); do case "$x" in zb.role=*) role=${x#zb.role=};; zb.quarantine=1) mode=quarantine-test;; zb.ats_quarantine=1) mode=ats-quarantine-test;; zb.lifetime=1) mode=lifetime;; zb.live=1) mode=serve;; zb.capacity=*) capacity=${x#zb.capacity=};; esac; done
echo "ZB_BOOT role=$role"
uname -a
fail() { echo "ZB_GUEST_FAIL $role"; dmesg | tail -80; poweroff -f; }
for m in $(cat /modules/order); do insmod /modules/$m.ko || fail; done
insmod /modules/zettbridge.ko || fail
if [ "$role" = consumer ] && [ "$mode" != lifetime ]; then
    sleep 2
    cxl list -D -M
    cxl create-region -d decoder0.0 -m mem0 -s "$capacity" -t ram -w 1 || fail
    sleep 1
    echo dax0.0 > /sys/bus/dax/drivers/zettbridge/new_id || fail
    ls -l /sys/bus/dax/devices/
fi
if [ "$mode" = serve ]; then
    (
        zbctl $role-serve || fail
    ) &
    echo $! > /run/zettbridge-service.pid
    echo "ZB_CONSOLE_READY $role"
    exec /bin/busybox cttyhack /bin/sh
fi
zbctl $role-$mode || fail
if [ "$mode" = lifetime ]; then
    # Re-register, then unload with a worker queued and no open file.
    insmod /modules/zettbridge.ko || fail
    rmmod zettbridge || fail
    echo "RUST RELOAD_WORKER_UNLOAD_PASS"
fi
echo "ZB_GUEST_PASS $role"
# Keep QEMU alive after successful detach so the peer can finish RETIRE.
sleep 5
poweroff -f
'''
 # tail is used only in diagnostics.
 (root/'bin/tail').symlink_to('busybox')
 (root/'init').write_text(init);(root/'init').chmod(0o755)
 names=subprocess.check_output(['find','.','-print0'],cwd=root)
 archive=subprocess.check_output(['cpio','--null','-o','--format=newc','--quiet'],input=names,cwd=root)
 import gzip
 pathlib.Path(a.output).write_bytes(gzip.compress(archive,compresslevel=1,mtime=0))
print(a.output)
