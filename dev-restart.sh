#!/data/data/com.termux/files/usr/bin/bash
# 开发辅助：杀掉旧的 music-service 并启动新编译的二进制（仅本地调试用）
cd "$(dirname "$0")" || exit 1

python3 - <<'EOF'
import os, signal, time
me = os.getpid()
for pid in os.listdir('/proc'):
    if not pid.isdigit() or int(pid) == me:
        continue
    try:
        with open(f'/proc/{pid}/cmdline', 'rb') as f:
            cmd = f.read().replace(b'\0', b' ').decode('utf-8', 'ignore').strip()
        # 只匹配真正的服务进程，避免误杀 shell 包装进程
        if cmd == './target/debug/music-service' or cmd.endswith('service/target/debug/music-service'):
            os.kill(int(pid), signal.SIGTERM)
            print('killed old server pid', pid)
    except Exception:
        pass
EOF
sleep 1

./target/debug/music-service > server-run.log 2>&1 &
sleep 2
curl -s -m 5 http://127.0.0.1:8080/api/v1/health && echo || echo "server not responding, log:"
tail -5 server-run.log
