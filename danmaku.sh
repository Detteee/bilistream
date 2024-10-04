#!/bin/bash

# 根据直播标题检查分区ID
check_area_id_with_title() {
  local live_title=$1
  local area_id=$2
  # Case insensitive match
  shopt -s nocasematch
  if [[ "$live_title" == *"Valorant"* ]]; then
    area_id=329
  elif [[ "$live_title" == *"League of Legends"* ]]; then
    area_id=86
  elif [[ "$live_title" == *"LOL"* ]]; then
    area_id=86
  elif [[ "$live_title" == *"k4sen"* ]]; then
    area_id=86
  # elif [[ "$live_title" == *"Apex Legends"* ]]; then
  #   area_id=240
  # elif [[ "$live_title" == *"ApexLegends"* ]]; then
  #   area_id=240
  # elif [[ "$live_title" == *"Apex"* ]]; then
  #   area_id=240
  elif [[ "$live_title" == *"Minecraft"* ]]; then
    area_id=216
  elif [[ "$live_title" == *"マイクラ"* ]]; then
    area_id=216
  elif [[ "$live_title" == *"Overwatch"* ]]; then
    area_id=87
  elif [[ "$live_title" == *"Deadlock"* ]]; then
    area_id=927
  elif [[ "$live_title" == *"漆黒メインクエ"* ]]; then
    area_id=102
  fi
  # Reset nocasematch
  shopt -u nocasematch
  echo $area_id
}

# Function to check if a channel is in the allowed list and get the channel name
check_channel() {
  local platform=$1
  local channel_id=$2
  local file_path="./${platform}/${platform}_channels.txt"
  local channel_info=$(grep -i "\[$channel_id\]" "$file_path")
  if [ -n "$channel_info" ]; then
    echo "$channel_info" | sed -E 's/\((.*)\).*/\1/'
  else
    echo ""
  fi
}

# Function to check live status using get_live_status
check_live_status() {
  local platform=$1
  local channel_id=$2
  ./bilistream get-live-status "$platform" "$channel_id"
}

# Function to update config
update_config() {
  local platform=$1
  local channel_name=$2
  local channel_id=$3
  local config_path="./${platform}/config.yaml"
  sed -i "s/ChannelName:.*/ChannelName: \"$channel_name\"/" "$config_path"
  sed -i "s/ChannelId:.*/ChannelId: $channel_id/" "$config_path"
}

# Function to check Bilibili live status
check_bilibili_status() {
  local room_id=$(jq -r '.roomId' config.json)
  echo "Room ID: $room_id"
  check_live_status "bilibili" "$room_id"
}
# Main function to process danmaku
process_danmaku() {
  echo "Processing danmaku: $1"
  # Validate danmaku command format: %转播%平台%频道名%分区
  if [[ $1 == *"转播"* ]]; then
    echo "Danmaku command is valid. Processing..."
    # Replace full-width ％ with half-width %
    normalized_danmaku=$(echo "$1" | sed 's/％/%/g')
    platform=$(echo "$normalized_danmaku" | grep -oP '%转播%\K\w+')
    channel_name=$(echo "$normalized_danmaku" | grep -oP '%转播%\w+%\K[^%]+')
    area_name=$(echo "$normalized_danmaku" | grep -oP '%转播%\w+%[^%]+%\K[^%]+')
    echo "Platform: $platform, Channel Name: $channel_name, Area Name: $area_name"
    case $area_name in
    "英雄联盟") area_id=86 ;;
    "无畏契约") area_id=329 ;;
    "APEX英雄") area_id=240 ;;
    "守望先锋") area_id=87 ;;
    "萌宅领域") area_id=530 ;;
    "其他单机") area_id=235 ;;
    "其他网游") area_id=107 ;;
    "UP主日常") area_id=646 ;;
    "最终幻想14") area_id=102 ;;
    "格斗游戏") area_id=433 ;;
    "我的世界") area_id=216 ;;
    "DeadLock") area_id=927 ;;
    *)
      echo "Unknown area: $area_name"
      continue
      ;;
    esac
    if [ "$area_id" -eq 240 ]; then
      if [ "$channel_name" != "kamito" ]; then
        echo "only kamito is allowed to use apex area"
        continue
      fi
    fi

    if [[ "$platform" == "YT" || "$platform" == "TW" ]]; then
      channel_id=$(grep -i "\(${channel_name}\)" "./${platform}/${platform}_channels.txt" | grep -oP '\[(.*?)\]' | tr -d '[]')
      if [ -n "$channel_id" ]; then
        live_status=$(check_live_status "$platform" "$channel_id")

        if [[ "$live_status" != *"Not Live"* ]]; then
          echo "area_id: $area_id"
          config_path="./${platform}/config.yaml"
          new_title="【转播】${channel_name}"
          if [[ "$platform" == "YT" ]]; then
            # check live channel title contains area name
            live_title=$(yt-dlp -e "https://www.youtube.com/channel/${channel_id}/live")
            area_id=$(check_area_id_with_title "$live_title" "$area_id")
            sed -i "/Youtube:/,/Twitch:/ s|ChannelId: .*|ChannelId: ${channel_id}|" "$config_path"
            sed -i "/Youtube:/,/Twitch:/ s|ChannelName: .*|ChannelName: \"${channel_name}\"|" "$config_path"
          else # TW
            live_title=$(./bilistream get-live-title TW "$channel_id")
            area_id=$(check_area_id_with_title "$live_title" "$area_id")
            sed -i "/Twitch:/,$ s|ChannelId: .*|ChannelId: ${channel_id}|" "$config_path"
            sed -i "/Twitch:/,$ s|ChannelName: .*|ChannelName: \"${channel_name}\"|" "$config_path"
          fi
          sed -i "s|Title: .*|Title: \"${new_title}\"|" "$config_path"
          sed -i "s|Area_v2: .*|Area_v2: ${area_id}|" "$config_path"
          echo "Updated $platform channel: $channel_name ($channel_id)"
          # 冷却10秒
          sleep 10
          # Trigger streaming process by restarting the bilistream service
        else
          echo "Channel $channel_name ($channel_id) is not live on $platform"
        fi
      else
        echo "Channel $channel_name not found in allowed list for $platform"
      fi
    else
      echo "Unsupported platform: $platform"
    fi
  fi
}
# Main loop
while true; do
  bilibili_status=$(check_bilibili_status)
  if [[ "$bilibili_status" == *"Not Live"* ]]; then
    echo "Bilibili is not live. Starting/Continuing danmaku-cli..."

    # Start danmaku-cli in background
    ./danmaku-cli --config config.json | while IFS= read -r line; do
      process_danmaku "$line"

      # Check Bilibili status every 300 seconds
      if ((SECONDS % 300 == 0)); then
        current_status=$(check_bilibili_status)
        if [[ "$current_status" != *"Not Live"* ]]; then
          echo "Bilibili is now live. Stopping danmaku-cli..."
          pkill -f "danmaku-cli"
          # remove danmaku lock file
          rm -f ./danmaku.lock
          break
        fi
      fi
    done &

    # Wait for danmaku-cli to finish or be killed
    wait $!
  else
    echo "Bilibili is live. Waiting for 100 seconds before checking again..."
  fi

  sleep 300 # Wait for 300 seconds before checking Bilibili status again
done
