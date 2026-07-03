import os
import time

files = [f"r{i:02}.txt" for i in range(32)]
for f in files:
    with open(f, "wb") as h:
        h.write(b"x" * 4096)
for f in files:
    fd = os.open(f, os.O_RDONLY)
    os.fsync(fd)
    os.close(fd)

# settle: one full pass so write-time invalidations are behind us
for f in files:
    os.stat(f)
    fd = os.open(f, os.O_RDONLY)
    os.read(fd, 4096)
    os.close(fd)

# storm shape: stat + open/read/close per file; on unpatched kernels every
# close invalidates STATX_BLOCKS so every stat forces a sync GETATTR
t0 = time.perf_counter()
for _ in range(32):
    for f in files:
        os.stat(f)
        fd = os.open(f, os.O_RDONLY)
        os.read(fd, 4096)
        os.close(fd)
t1 = time.perf_counter()
print(f"storm: {(t1 - t0) / (32 * 32) * 1e6:.2f}us/cycle")

# correctness (the cf576c58b3a2 du case): st_blocks must be fresh after a
# buffered write + close, i.e. the patch must still invalidate for writers
g = "grow.bin"
with open(g, "wb") as h:
    h.write(b"")
os.stat(g)
with open(g, "ab") as h:
    h.write(b"z" * (1024 * 1024))
st = os.stat(g)
print(f"blocks-after-1MB-write: {st.st_blocks}")
assert st.st_blocks >= 2040, f"stale st_blocks {st.st_blocks}: du regression!"

# mmap variant of the same correctness check (page_mkwrite path)
import mmap
m = "mmapped.bin"
with open(m, "wb") as h:
    h.write(b"\0" * 8192)
os.stat(m)
fd = os.open(m, os.O_RDWR)
mm = mmap.mmap(fd, 8192)
mm[0:8192] = b"y" * 8192
mm.flush()
mm.close()
os.close(fd)
st = os.stat(m)
print(f"blocks-after-mmap-write: {st.st_blocks}")
assert st.st_blocks >= 16, f"stale st_blocks {st.st_blocks} after mmap write"
print("CORRECTNESS OK")
