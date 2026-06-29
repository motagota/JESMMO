import asyncio
import websockets
import json

async def run():
    uri = 'ws://localhost:8766'
    async with websockets.connect(uri) as ws:
        print('Connected to proxy')
        # wait a moment to ensure server sends any initial messages
        await asyncio.sleep(0.5)
        # send a move command
        await ws.send(json.dumps({'type': 'move', 'dx': 1, 'dy': 2}))
        print('Sent move')
        try:
            async for message in ws:
                print('Received:', message)
        except websockets.ConnectionClosed:
            print('Connection closed')

if __name__ == '__main__':
    asyncio.run(run())
