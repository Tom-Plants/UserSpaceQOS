#!/usr/bin/env python3
# ── QOS UDP 回声器（Linux/OpenWrt 端，监听 UDP 15390）──
# 识别 QOSPROBE（稳态）与 QOSBURST（微突发）前缀并原样回射。
import socket, sys
PORT = 15390
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("0.0.0.0", PORT))
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
print(f"qos_udp_echo 已启动, 监听 UDP {PORT}", flush=True)
while True:
    try:
        d, a = s.recvfrom(2048)
        if d.startswith(b"QOSPROBE") or d.startswith(b"QOSBURST"):
            s.sendto(d, a)
    except Exception as e:
        print(f"异常: {e}", file=sys.stderr, flush=True)
