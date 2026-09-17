# GAUGE r2 — TUI time-to-first-frame via ConPTY (pywinpty).
# Usage: python gauge_tui_startup.py <slim.exe> <cwd> <reps>
import queue
import sys
import threading
import time

from winpty import PtyProcess


def measure(binary, cwd, timeout=20.0, quiet_secs=1.0):
    t0 = time.perf_counter()
    proc = PtyProcess.spawn([binary], cwd=cwd, dimensions=(24, 80))
    q = queue.Queue()

    def reader():
        try:
            while proc.isalive():
                data = proc.read(65536)
                if data:
                    q.put(data)
                else:
                    break
        except Exception:
            pass
        q.put(None)

    threading.Thread(target=reader, daemon=True).start()
    first_byte_at = None
    quiet_at = None
    total = 0
    last_data = t0
    while True:
        now = time.perf_counter()
        if now - t0 > timeout:
            break
        if first_byte_at is not None and (now - last_data) > quiet_secs:
            quiet_at = last_data
            break
        try:
            item = q.get(timeout=0.1)
        except queue.Empty:
            continue
        if item is None:
            break
        if first_byte_at is None:
            first_byte_at = now
        total += len(item)
        last_data = now
        # Answer terminal queries like a real terminal would.
        if isinstance(item, bytes):
            item = item.decode("utf-8", "replace")
        if "\x1b[c" in item:
            try:
                proc.write("\x1b[?1;2c")
            except Exception:
                pass
        if "\x1b[1t" in item:
            try:
                proc.write("\x1b[4;600;800t")
            except Exception:
                pass
        if "\x1b[6n" in item:
            try:
                proc.write("\x1b[1;1R")
            except Exception:
                pass
    try:
        proc.terminate(force=True)
    except Exception:
        pass
    return (
        (first_byte_at - t0) if first_byte_at is not None else None,
        (quiet_at - t0) if quiet_at is not None else None,
        total,
    )


def main():
    binary, cwd, reps = sys.argv[1], sys.argv[2], int(sys.argv[3])
    firsts, quiets = [], []
    for _ in range(reps):
        fb, qu, total = measure(binary, cwd)
        if fb is None:
            print("no output")
            continue
        firsts.append(fb * 1000)
        if qu is not None:
            quiets.append(qu * 1000)
        qtxt = f"{qu*1000:.1f}ms" if qu is not None else "timeout"
        print(f"first_byte={fb*1000:.1f}ms quiet={qtxt} bytes={total}")
    if firsts:
        firsts.sort()
        print(f"median_first_byte={firsts[len(firsts)//2]:.1f}ms")
        if quiets:
            quiets.sort()
            print(f"median_quiet={quiets[len(quiets)//2]:.1f}ms")


if __name__ == "__main__":
    main()
