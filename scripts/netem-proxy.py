#!/usr/bin/env python3
"""弱网模拟代理：夹在客户端与 SSH 服务器之间，给每条连接加上延迟、限速和随机断线。

用途是在本机复现「网络很差」时的表现并做对比测试，不需要管理员权限（不改系统的网络配置）。
它在 TCP 之上工作，所以只能模拟这三件事：

  --delay-ms   每个方向的固定延迟（往返延迟是它的两倍）
  --rate-kbps  每条连接、每个方向的带宽上限
  --kill-every 每条连接平均存活多少秒后被掐断（指数分布；0 表示不掐）
  --stall-every 每条连接平均多少秒后变成「黑洞」：不再转发任何数据，但也不断开（0 表示不发生）

模拟不了的：真实丢包下 TCP 拥塞控制的动态。「每条连接各自限速」是对丢包链路的一个近似——
丢包时单条 TCP 的吞吐被压得很低、而多条连接各自都能拿到差不多的吞吐，这正是并行传输想利用的现象。
因此这里测出的并行加速比反映的是引擎本身的开销与伸缩性，不能直接当作真实网络上的成绩。

    scripts/netem-proxy.py --listen 2300 --target 2299 --delay-ms 50 --rate-kbps 8000
"""
import argparse
import asyncio
import random
import time

CHUNK_BYTES = 16 * 1024
# 变成黑洞的连接保持多久才真正关闭：足够长，让客户端只能靠自己的超时或检测发现它。
STALL_HOLD_SECS = 600
# 排队中的数据块上限：到了就暂停读取，把拥堵反压回发送方，而不是无限占用内存。
QUEUE_CHUNKS = 64


async def pump(reader, writer, delay, rate):
    """单向转发：按速率读、延迟后按原顺序写。"""
    queue = asyncio.Queue(QUEUE_CHUNKS)

    async def deliver():
        while True:
            ready_at, data = await queue.get()
            if data is None:
                break
            wait = ready_at - time.monotonic()
            if wait > 0:
                await asyncio.sleep(wait)
            writer.write(data)
            await writer.drain()

    sender = asyncio.create_task(deliver())
    try:
        while True:
            data = await reader.read(CHUNK_BYTES)
            if not data:
                break
            await queue.put((time.monotonic() + delay, data))
            if rate:
                await asyncio.sleep(len(data) / rate)
    finally:
        await queue.put((0, None))
        await sender


async def handle(client_reader, client_writer, args):
    try:
        server_reader, server_writer = await asyncio.open_connection("127.0.0.1", args.target)
    except OSError:
        client_writer.close()
        return
    delay = args.delay_ms / 1000
    rate = args.rate_kbps * 1000 / 8
    tasks = [
        asyncio.create_task(pump(client_reader, server_writer, delay, rate)),
        asyncio.create_task(pump(server_reader, client_writer, delay, rate)),
    ]
    if args.kill_every > 0:
        tasks.append(asyncio.create_task(asyncio.sleep(random.expovariate(1 / args.kill_every))))
    stall = None
    if args.stall_every > 0:
        stall = asyncio.create_task(asyncio.sleep(random.expovariate(1 / args.stall_every)))
        tasks.append(stall)
    try:
        await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        if stall is not None and stall.done():
            # 黑洞：两个方向都停止转发，连接本身留着不关。
            for task in tasks[:2]:
                task.cancel()
            await asyncio.sleep(STALL_HOLD_SECS)
    finally:
        for task in tasks:
            task.cancel()
        # 先断开、再等各方向收尾。顺序反过来的话，收尾时还在排队的数据要先发完，期间这条连接
        # 既没断、另一个方向又已经停止转发，成了一个「黑洞」——那是另一种故障，不是这里要模拟的断线。
        for writer in (client_writer, server_writer):
            # abort 而不是 close：模拟的是「连接断了」，不是对端正常关闭。
            writer.transport.abort()
        # 吞掉收尾时的异常：一端断开后另一端的写入必然报错，这正是要模拟的情况，不是代理的故障。
        await asyncio.gather(*tasks, return_exceptions=True)


async def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--listen", type=int, required=True)
    parser.add_argument("--target", type=int, required=True)
    parser.add_argument("--delay-ms", type=float, default=0)
    parser.add_argument("--rate-kbps", type=float, default=0)
    parser.add_argument("--kill-every", type=float, default=0)
    parser.add_argument("--stall-every", type=float, default=0)
    args = parser.parse_args()
    server = await asyncio.start_server(lambda r, w: handle(r, w, args), "127.0.0.1", args.listen)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
