
import asyncio
import websockets
import sys
import json
import numpy as np
from gymnasium import spaces

WORLD_WIDTH = 600
WORLD_HEIGHT = 400
BOT_MOVE_INTERVAL = 1.0
TICK_RATE = 20            # authoritative simulation ticks per second
TICK_DT = 1.0 / TICK_RATE

class ZoneBotEnv:
    def __init__(self, width=WORLD_WIDTH, height=WORLD_HEIGHT, step_size=10):
        self.width = width
        self.height = height
        self.step_size = step_size
        self.observation_space = spaces.Box(
            low=np.array([0, 0], dtype=np.int32),
            high=np.array([self.width, self.height], dtype=np.int32),
            dtype=np.int32
        )
        self.action_space = spaces.Discrete(5)
        self.state = np.array([0, 0], dtype=np.int32)

    def reset(self, state=None):
        if state is None:
            self.state = np.array([0, 0], dtype=np.int32)
        else:
            self.state = np.array(state, dtype=np.int32)
        return self.state, {}

    def step(self, action):
        dx, dy = 0, 0
        if action == 1:
            dy = -self.step_size
        elif action == 2:
            dy = self.step_size
        elif action == 3:
            dx = -self.step_size
        elif action == 4:
            dx = self.step_size

        self.state[0] = np.clip(self.state[0] + dx, 0, self.width)
        self.state[1] = np.clip(self.state[1] + dy, 0, self.height)
        reward = 0.0
        terminated = False
        truncated = False
        info = {}
        return self.state.copy(), reward, terminated, truncated, info

class ZoneBot:
    SPEED = 40  # units per second (-> 2 units/tick at 20Hz, smooth integer motion)

    DIRS = {0: (0, 0), 1: (0, -1), 2: (0, 1), 3: (-1, 0), 4: (1, 0)}

    def __init__(self, bot_id, env, start_position):
        self.bot_id = bot_id
        self.env = env
        self.x = float(start_position[0])
        self.y = float(start_position[1])
        self.vx = 0.0
        self.vy = 0.0
        self.choose_velocity()

    def choose_velocity(self):
        """Pick a new heading (re-evaluated every decision interval)."""
        ux, uy = self.DIRS[self.env.action_space.sample()]
        self.vx = ux * self.SPEED
        self.vy = uy * self.SPEED

    def step(self, dt, width, height):
        """Integrate velocity for one tick. Returns True if the bot moved."""
        nx = max(0, min(width, self.x + self.vx * dt))
        ny = max(0, min(height, self.y + self.vy * dt))
        moved = nx != self.x or ny != self.y
        self.x, self.y = nx, ny
        return moved

class ZoneServer:
    def __init__(self, zone_id, port, proxy_uri=None):
        self.zone_id = zone_id
        self.port = port
        self.proxy_uri = proxy_uri
        self.players = {}
        self.bots = {}
        self.proxy_ws = None
        self.bot_counter = 0
        # player_id -> [dx, dy] accumulated since the last tick
        self.pending = {}

    async def handle_proxy(self, websocket):
        self.proxy_ws = websocket
        print(f"[Zone {self.zone_id}] Proxy connected")

        try:
            async for message in websocket:
                data = json.loads(message)
                msg_type = data.get('type')
                player_id = data.get('player_id')

                if msg_type == 'player_join':
                    self.players[player_id] = {'x': 0, 'y': 0, 'hp': 100, 'type': 'player'}
                    print(f"[Zone {self.zone_id}] Player joined: {player_id}")
                    await self._send_status_update(player_id)
                    # Send current bot state to the joining player
                    for bot_id, bot_data in self.players.items():
                        if bot_data.get('type') == 'bot':
                            await self._send_status_update(bot_id)
                elif msg_type == 'player_leave':
                    self.players.pop(player_id, None)
                    print(f"[Zone {self.zone_id}] Player left: {player_id}")
                elif msg_type == 'move':
                    # Buffer the input; it's applied on the next simulation tick.
                    if player_id in self.players:
                        dx = data.get('dx', 0)
                        dy = data.get('dy', 0)
                        acc = self.pending.setdefault(player_id, [0, 0])
                        acc[0] += dx
                        acc[1] += dy

        except websockets.ConnectionClosed:
            print(f"[Zone {self.zone_id}] Proxy disconnected")
    
    async def register_with_proxy(self):
        """Register this zone with the proxy server"""
        if not self.proxy_uri:
            print(f"[Zone {self.zone_id}] No proxy URI provided, skipping registration")
            return
        
        try:
            zone_uri = f'ws://localhost:{self.port}'
            registration_msg = {
                'type': 'register_zone',
                'zone_id': self.zone_id,
                'uri': zone_uri
            }
            
            async with websockets.connect(self.proxy_uri) as proxy_ws:
                await proxy_ws.send(json.dumps(registration_msg))
                print(f"[Zone {self.zone_id}] Registered with proxy at {self.proxy_uri}")
                
                while True:
                    await asyncio.sleep(30)
                    try:
                        await proxy_ws.ping()
                    except Exception:
                        print(f"[Zone {self.zone_id}] Proxy connection lost, attempting re-registration")
                        break
        except Exception as e:
            print(f"[Zone {self.zone_id}] Failed to register with proxy: {e}")
            print(f"[Zone {self.zone_id}] Will retry in 5 seconds...")
            await asyncio.sleep(5)
            await self.register_with_proxy()

    def _bounded_position(self, x, y):
        x = max(0, min(WORLD_WIDTH, x))
        y = max(0, min(WORLD_HEIGHT, y))
        return x, y

    async def _send_status_update(self, player_id):
        if self.proxy_ws and player_id in self.players:
            state = {
                'x': self.players[player_id]['x'],
                'y': self.players[player_id]['y'],
                'hp': self.players[player_id]['hp'],
                'type': self.players[player_id].get('type', 'player')
            }
            packet = {
                'type': 'status_update',
                'player_id': player_id,
                'state': state
            }
            await self.proxy_ws.send(json.dumps(packet))

    def _spawn_bot(self):
        bot_id = f'bot_{self.bot_counter}'
        self.bot_counter += 1
        env = ZoneBotEnv()
        x, y = self._bounded_position(
            np.random.randint(0, WORLD_WIDTH + 1),
            np.random.randint(0, WORLD_HEIGHT + 1)
        )
        self.bots[bot_id] = ZoneBot(bot_id, env, (x, y))
        self.players[bot_id] = {
            'x': x,
            'y': y,
            'hp': 100,
            'type': 'bot'
        }
        print(f"[Zone {self.zone_id}] Spawned bot {bot_id} at ({x}, {y})")

    async def _game_loop(self):
        """Authoritative fixed-rate simulation. Applies buffered input, steps
        bots, and broadcasts only the entities that changed this tick."""
        bot_accum = 0.0
        while True:
            await asyncio.sleep(TICK_DT)
            if not self.proxy_ws:
                continue

            dirty = set()

            # 1. Apply buffered player input.
            for player_id, (dx, dy) in self.pending.items():
                if player_id in self.players and (dx or dy):
                    self.players[player_id]['x'], self.players[player_id]['y'] = self._bounded_position(
                        self.players[player_id]['x'] + dx,
                        self.players[player_id]['y'] + dy
                    )
                    dirty.add(player_id)
            self.pending.clear()

            # 2. Step bots every tick for smooth motion; re-pick heading periodically.
            bot_accum += TICK_DT
            repick = bot_accum >= BOT_MOVE_INTERVAL
            if repick:
                bot_accum -= BOT_MOVE_INTERVAL
            for bot_id, bot in list(self.bots.items()):
                if repick:
                    bot.choose_velocity()
                if bot.step(TICK_DT, WORLD_WIDTH, WORLD_HEIGHT):
                    self.players[bot_id]['x'] = int(round(bot.x))
                    self.players[bot_id]['y'] = int(round(bot.y))
                    dirty.add(bot_id)

            # 3. Broadcast the entities that moved.
            for player_id in dirty:
                await self._send_status_update(player_id)
    
    async def start(self):
        print(f"[Zone {self.zone_id}] Starting on port {self.port}")
        
        if self.proxy_uri:
            asyncio.create_task(self.register_with_proxy())

        # Spawn 3 bots when the zone starts
        for _ in range(3):
            self._spawn_bot()
        asyncio.create_task(self._game_loop())
        
        async with websockets.serve(self.handle_proxy, 'localhost', self.port):
            await asyncio.Future()  # run forever:

if __name__ == '__main__':
    zone_id = sys.argv[1] if len(sys.argv) > 1 else 'zone_default'
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 9001
    proxy_uri = sys.argv[3] if len(sys.argv) > 3 else None
    server = ZoneServer(zone_id, port, proxy_uri)
    asyncio.run(server.start())