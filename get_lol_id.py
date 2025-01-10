from riotwatcher import LolWatcher
import sys

if len(sys.argv) != 3:
    print("Usage: python3 get_lol_id.py <api_key> <puuid>")
    sys.exit(1)

api_key = sys.argv[1]
puuid = sys.argv[2]
# riot_watcher = RiotWatcher(api_key)
lol_watcher = LolWatcher(api_key)

# Fetch account information by Riot ID
# riot_acc = riot_watcher.account.by_riot_id('asia', 'Kamito', '8595')
# print(riot_acc['puuid'])

# Get current game information for the summoner
game_data = lol_watcher.spectator.by_summoner('jp1', puuid)

riot_ids = [participant['riotId'] for participant in game_data['participants']]
print(riot_ids)



