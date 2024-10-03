#!/bin/bash

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
  check_live_status "bilibili" "$room_id"
}
# Main function to process danmaku
process_danmaku() {
  echo "$1"
  if [[ $1 == *"%转播%"* ]]; then
    platform=$(echo "$1" | grep -oP '%转播%\K\w+')
    channel_name=$(echo "$1" | grep -oP '%转播%\w+%\K[^%]+')

    if [[ "$platform" == "YT" || "$platform" == "TW" ]]; then
      channel_id=$(grep -i "\(${channel_name}\)" "./${platform}/${platform}_channels.txt" | grep -oP '\[(.*?)\]' | tr -d '[]')
      if [ -n "$channel_id" ]; then
        live_status=$(check_live_status "$platform" "$channel_id")
        if [[ "$live_status" != *"Not Live"* ]]; then
          config_path="./${platform}/config.yaml"
          new_title="【转播】${channel_name}"
          if [[ "$platform" == "YT" ]]; then
            sed -i "/Youtube:/,/Twitch:/ s|ChannelId: .*|ChannelId: ${channel_id}|" "$config_path"
            sed -i "s|ChannelName: .*|ChannelName: \"${channel_name}\"|" "$config_path"
          else # TW
            sed -i "/Twitch:/,$ s|ChannelId: .*|ChannelId: ${channel_id}|" "$config_path"
            sed -i "/Twitch:/,$ s|ChannelName: .*|ChannelName: \"${channel_name}\"|" "$config_path"
          fi
          sed -i "s|Title: .*|Title: \"${new_title}\"|" "$config_path"
          echo "Updated $platform channel: $channel_name ($channel_id)"
          sleep 30
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
    ./bilibili-live-danmaku-cli --config config.json | while IFS= read -r line; do
      process_danmaku "$line"

      # Check Bilibili status every 300 seconds
      if ((SECONDS % 300 == 0)); then
        current_status=$(check_bilibili_status)
        if [[ "$current_status" != *"Not Live"* ]]; then
          echo "Bilibili is now live. Stopping danmaku-cli..."
          pkill -f "bilibili-live-danmaku-cli"
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
