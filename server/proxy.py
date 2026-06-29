import asyncio
import json
import websockets
import uuid

class ZoneConnection:
    def __init__(self, uri , zone_id ):
        self.uri = uri
        self.zone_id = zone_id
        self.ws = None
        self.migration_state = None  # None, 'marking', 'migrating', 'retired'
        self.packet_buffer = {}  # player_id -> list of buffered messages

class ProxyServer:
    def __init__(self, host='localhost', port=8766, registration_port=8764):
        self.host = host
        self.port = port
        self.registration_port = registration_port
        self.clients = {}
        self.zones = {}

    async def register_zone(self, zone_id, uri):
        ws = await websockets.connect(uri)
        zone = ZoneConnection(uri, zone_id)
        zone.ws = ws
        self.zones[zone_id] = zone
        asyncio.create_task(self._zone_listener(zone))
        print(f"[Proxy] Registered zone {zone_id} at {uri}")
        return zone;

    async def handle_zone_registration(self, websocket):
        """Handle zone registration requests"""
        try:
            async for message in websocket:
                data = json.loads(message)
                if data.get('type') == 'register_zone':
                    zone_id = data.get('zone_id')
                    uri = data.get('uri')
                    if not zone_id or not uri:
                        print(f"[Proxy] Invalid zone registration payload: {data}")
                        continue

                    if zone_id not in self.zones:
                        await self.register_zone(zone_id, uri)
                        print(f"[Proxy] Zone {zone_id} self-registered from {uri}")
                    else:
                        print(f"[Proxy] Zone {zone_id} already registered")
        except websockets.ConnectionClosed:
            pass
        except Exception as e:
            print(f"[Proxy] Zone registration error: {e}")

    async def start_registration_service(self):
        """Start the zone registration service"""
        print(f"[Proxy] Zone registration service listening on ws://{self.host}:{self.registration_port}")
        async with websockets.serve(self.handle_zone_registration, self.host, self.registration_port):
            await asyncio.Future()  # run forever

    async def _zone_listener(self, zone):
        try:
            async for message in zone.ws:
                data = json.loads(message)
                msg_type = data.get('type')
                player_id = data.get('player_id')

                # Forward status updates to every client in the same zone.
                # Bots are not directly connected clients, so they must be broadcast.
                if msg_type == 'status_update':
                    print(f"[Proxy] Broadcasting bot status_update from {player_id} to zone clients")
                    for ws, info in list(self.clients.items()):
                        if info.get('current_zone') == zone:
                            try:
                                await ws.send(json.dumps(data))
                            except Exception as e:
                                print(f"[Proxy] Failed to send status_update for {player_id} to client: {e}")
                elif player_id:
                    # For other zone-to-client messages, route only to the matching player.
                    for ws, info in list(self.clients.items()):
                        if info.get('player_id') == player_id and info.get('current_zone') == zone:
                            try:
                                await ws.send(json.dumps(data))
                            except Exception as e:
                                print(f"[Proxy] Failed to send to client {player_id}: {e}")
        except websockets.ConnectionClosed:
            print(f"[Proxy] Zone {zone.zone_id} disconnected")
        except Exception as e:
            print(f"[Proxy] Zone {zone.zone_id} connection error: {e}")

    async def phase1_mark_for_migration(self, zone_id):
        """Phase 1: Mark zone for migration and start buffering client packets"""
        if zone_id not in self.zones:
            print(f"[Proxy] Zone {zone_id} not found")
            return False
        
        zone = self.zones[zone_id]
        zone.migration_state = 'marking'
        print(f"[Proxy] PHASE 1: Zone {zone_id} marked for migration - buffering client packets")
        
        # Initialize packet buffers for all players in this zone
        for ws, info in self.clients.items():
            if info.get('current_zone') == zone:
                player_id = info['player_id']
                zone.packet_buffer[player_id] = []
                print(f"[Proxy] Buffering enabled for player {player_id}")
        
        return True

    async def phase2_transfer_players(self, source_zone_id, target_zone_id):
        """Phase 2: Transfer each player from source zone to target zone"""
        if source_zone_id not in self.zones or target_zone_id not in self.zones:
            print(f"[Proxy] Source or target zone not found")
            return False
        
        source_zone = self.zones[source_zone_id]
        target_zone = self.zones[target_zone_id]
        
        source_zone.migration_state = 'migrating'
        print(f"[Proxy] PHASE 2: Transferring players from {source_zone_id} to {target_zone_id}")
        
        # Find all players in source zone
        players_to_migrate = []
        for ws, info in self.clients.items():
            if info.get('current_zone') == source_zone:
                players_to_migrate.append((ws, info['player_id']))
        
        # Transfer each player
        for ws, player_id in players_to_migrate:
            try:
                # Notify source zone of player leave
                await source_zone.ws.send(json.dumps({
                    'type': 'player_leave',
                    'player_id': player_id
                }))
                print(f"[Proxy] Player {player_id} leaving {source_zone_id}")
                
                # Give source zone time to clean up
                await asyncio.sleep(0.1)
                
                # Get buffered packets
                buffered = source_zone.packet_buffer.pop(player_id, [])
                
                # Update client's zone reference
                for client_ws, info in self.clients.items():
                    if info['player_id'] == player_id:
                        info['current_zone'] = target_zone
                        break
                
                # Notify target zone of player join
                await target_zone.ws.send(json.dumps({
                    'type': 'player_join',
                    'player_id': player_id
                }))
                print(f"[Proxy] Player {player_id} joined {target_zone_id}")
                
                # Replay buffered packets on target zone
                for buffered_msg in buffered:
                    await target_zone.ws.send(json.dumps(buffered_msg))
                    print(f"[Proxy] Replayed buffered packet for {player_id}")
                
                # Send migration notification to client
                await ws.send(json.dumps({
                    'type': 'zone_migration',
                    'zone': target_zone_id,
                    'message': f'Migrated to {target_zone_id}'
                }))
                
            except Exception as e:
                print(f"[Proxy] Migration error for player {player_id}: {e}")
        
        return True

    async def phase3_retire_zone(self, zone_id):
        """Phase 3: Retire the old zone"""
        if zone_id not in self.zones:
            print(f"[Proxy] Zone {zone_id} not found")
            return False
        
        zone = self.zones[zone_id]
        zone.migration_state = 'retired'
        print(f"[Proxy] PHASE 3: Zone {zone_id} retired")
        
        # Close connection to retired zone
        if zone.ws:
            await zone.ws.close()
            print(f"[Proxy] Closed connection to {zone_id}")
        
        return True

    async def handle_client(self, websocket):
        wait_start = asyncio.get_event_loop().time()
        while not self.zones and asyncio.get_event_loop().time() - wait_start < 5:
            await asyncio.sleep(0.1)

        if not self.zones:
            await websocket.close(code=1011, reason='No zones available')
            print(f"[Proxy] Rejecting client because no zones are registered")
            return

        player_id = str(uuid.uuid4())
        default_zone = list(self.zones.values())[0]

        self.clients[websocket] = {
            'player_id': player_id,
            'current_zone': default_zone
        }

        await default_zone.ws.send(json.dumps({
            'type': 'player_join',
            'player_id': player_id
        }))

        print(f"[Proxy] Client connected: {player_id} -> {default_zone.zone_id} ({default_zone.uri})")

        try:
            async for message in websocket:
                info = self.clients.get(websocket)
                if not info:
                    # Client info missing; stop processing messages for this websocket
                    break
                data = json.loads(message)
                data['player_id'] = player_id
                
                current_zone = info['current_zone']
                
                # If zone is in migration state, buffer the packet instead of sending
                if current_zone.migration_state == 'marking':
                    if player_id not in current_zone.packet_buffer:
                        current_zone.packet_buffer[player_id] = []
                    current_zone.packet_buffer[player_id].append(data)
                    print(f"[Proxy] Buffered packet for {player_id}: {data.get('type')}")
                else:
                    # Normal operation - forward to zone
                    await current_zone.ws.send(json.dumps(data))
        except websockets.ConnectionClosed:
            pass
        finally:
            info = self.clients.pop(websocket, None)
            if info and info.get('current_zone') and info['current_zone'].ws:
                await info['current_zone'].ws.send(json.dumps({
                    'type': 'player_leave',
                    'player_id': player_id
                }))
                print(f"[Proxy] Client disconnected: {player_id} from {info['current_zone'].zone_id}")

    async def command_listener(self):
        loop = asyncio.get_event_loop()
        while True:
            try:
                line = await loop.run_in_executor(None, input)
                parts = line.strip().split()
                
                if len(parts) >= 2 and parts[0] == 'migrate':
                    if parts[1] == 'phase1' and len(parts) == 3:
                        # Phase 1: mark source zone for migration
                        source_zone = parts[2]
                        await self.phase1_mark_for_migration(source_zone)
                    
                    elif parts[1] == 'phase2' and len(parts) == 4:
                        # Phase 2: transfer players from source to target
                        source_zone = parts[2]
                        target_zone = parts[3]
                        await self.phase2_transfer_players(source_zone, target_zone)
                    
                    elif parts[1] == 'phase3' and len(parts) == 3:
                        # Phase 3: retire the source zone
                        source_zone = parts[2]
                        await self.phase3_retire_zone(source_zone)
                    
                    elif parts[1] == 'auto' and len(parts) == 4:
                        # Auto-execute all 3 phases
                        source_zone = parts[2]
                        target_zone = parts[3]
                        print(f"[Proxy] Starting automated 3-phase migration from {source_zone} to {target_zone}")
                        
                        # Phase 1
                        if await self.phase1_mark_for_migration(source_zone):
                            await asyncio.sleep(1)
                            
                            # Phase 2
                            if await self.phase2_transfer_players(source_zone, target_zone):
                                await asyncio.sleep(1)
                                
                                # Phase 3
                                await self.phase3_retire_zone(source_zone)
                                print(f"[Proxy] Migration complete!")
                    else:
                        self._print_migration_help()
                else:
                    self._print_migration_help()
            except Exception as e:
                print(f"[Proxy] Command error: {e}")

    def _print_migration_help(self):
        print("[Proxy] Migration commands:")
        print("  migrate phase1 <zone_id>                    - Phase 1: Mark zone for migration")
        print("  migrate phase2 <source_zone> <target_zone>  - Phase 2: Transfer players")
        print("  migrate phase3 <zone_id>                    - Phase 3: Retire zone")
        print("  migrate auto <source_zone> <target_zone>    - Execute all 3 phases automatically")

    async def start(self):
        print(f"[Proxy] Listening for clients on ws://{self.host}:{self.port}")
        print(f"[Proxy] Zone registration service on ws://{self.host}:{self.registration_port}")
        print("[Proxy] Migration commands: migrate phase1 <zone> | migrate phase2 <src> <tgt> | migrate phase3 <zone> | migrate auto <src> <tgt>")
        
        # Start both services in parallel
        async with websockets.serve(self.handle_client, self.host, self.port):
            async with websockets.serve(self.handle_zone_registration, self.host, self.registration_port):
                await self.command_listener()

    async def stop(self):
        print("Stopping proxy server")
        # Here you would add the logic to stop the proxy server


if __name__ == "__main__":
    proxy = ProxyServer()
    asyncio.run(proxy.start())