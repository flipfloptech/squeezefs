source scripts/suites/common.sh
set +e +o pipefail
suite_prepare probe >/dev/null 2>&1
suite_mount lk3 >/dev/null 2>&1; m=${SUITE_MNT[lk3]}
f=$m/lockfile; echo x > $f; echo "ino $(stat -c %i $f)"
exec {fd}<> $f; flock -x $fd; echo "flock -x fd rc=$?"; grep "00:" /proc/locks | grep -v "00:07"; echo "--- strace:"; strace -f -e trace=flock flock -x $fd 2>&1 | head -3
flock -x $f -c "grep 00: /proc/locks | grep -v 00:07; echo inner-done"
exec {fd}>&-
cp $f /build/tmp/lk-ext4; exec {fd2}<> /build/tmp/lk-ext4; flock -x $fd2; echo "ext4 control: $(grep -c "fe:20:$(stat -c %i /build/tmp/lk-ext4) " /proc/locks) row(s)"; exec {fd2}>&-; rm -f /build/tmp/lk-ext4
suite_umount lk3 >/dev/null 2>&1; echo RESULTS=$RESULTS
