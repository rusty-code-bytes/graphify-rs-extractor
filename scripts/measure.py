"""Measure one child run, including peak RSS, without platform-specific `time` flags."""
import json
import resource
import subprocess
import sys
import time

started = time.monotonic()
result = subprocess.run(sys.argv[1:], check=False)
usage = resource.getrusage(resource.RUSAGE_CHILDREN)
rss_bytes = usage.ru_maxrss if sys.platform == "darwin" else usage.ru_maxrss * 1024
print(json.dumps({"exit_code": result.returncode, "wall_seconds": time.monotonic() - started,
                  "user_seconds": usage.ru_utime, "system_seconds": usage.ru_stime,
                  "peak_rss_mib": rss_bytes / 1024**2}, indent=2))
sys.exit(result.returncode)
