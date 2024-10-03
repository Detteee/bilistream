#!/bin/bash

# Set BASE_DIR to the bilistream and stream_manager.sh working directory
BASE_DIR=$(pwd)

# Define colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
PINK='\033[38;5;218m'
RESET='\033[0m'

# Function to get the full path of the config file
get_config_path() {
    local service=$1
    echo "$BASE_DIR/$service/config.yaml"
}

# Function to start a service
start_service() {
    local service=$1
    local config_path=$(get_config_path "$service")
    local log_path="$BASE_DIR/logs/log-$service.log"
    echo "Starting bilistream for $service..."
    pm2 start "$BASE_DIR/bilistream" --name "bilistream-$service" -- -c "$config_path"
}

# Function to restart a service
restart_service() {
    local service=$1
    echo "Restarting bilistream for $service..."
    pm2 restart "bilistream-$service"
}

# Function to check if a service is running
is_service_running() {
    local service=$1
    pm2 list | grep -q "bilistream-$service"
    return $?
}

# Function to manage PM2 service
manage_pm2_service() {
    local service=$1
    local action

    if is_service_running "$service"; then
        echo "bilistream-$service is currently running."
        echo "1. Restart service"
        echo "2. Stop service"
        echo "3. Delete service"
        echo "4. Do nothing"
        read -p "Enter your choice (1/2/3/4): " action

        case $action in
        1) restart_service "$service" ;;
        2)
            echo "Stopping bilistream-$service..."
            pm2 stop "bilistream-$service"
            ;;
        3)
            echo "Deleting bilistream-$service..."
            pm2 delete "bilistream-$service"
            ;;
        4) echo "No action taken." ;;
        *) echo "Invalid choice. No action taken." ;;
        esac
    else
        echo "bilistream-$service is not running."
        read -p "Start bilistream-$service? (y/N):" action
        if [[ $action =~ ^[Yy]$ ]]; then
            start_service "$service"
        else
            echo "bilistream-$service was not started."
        fi
    fi
}

# Function to manage all PM2 services
manage_all_pm2_services() {
    echo "Managing all PM2 services..."
    local action

    echo "1. Start/Restart all services"
    echo "2. Stop all services"
    echo "3. Delete all services"
    echo "4. Do nothing"
    read -p "Enter your choice (1/2/3/4): " action

    case $action in
    1)
        for service in YT TW; do
            if is_service_running "$service"; then
                restart_service "$service"
            else
                start_service "$service"
            fi
        done
        ;;
    2)
        echo "Stopping all bilistream services..."
        pm2 stop "bilistream-YT" "bilistream-TW"
        ;;
    3)
        echo "Deleting all bilistream services..."
        pm2 delete "bilistream-YT" "bilistream-TW"
        ;;
    4) echo "No action taken." ;;
    *) echo "Invalid choice. No action taken." ;;
    esac
}

# Function to update kamito across all configs
update_kamito() {
    # Update TW config
    sed -i 's/ChannelId: .*/ChannelId: kamito_jp/' "$BASE_DIR/TW/config.yaml"
    sed -i 's/ChannelName: .*/ChannelName: "kamito"/' "$BASE_DIR/TW/config.yaml"
    sed -i 's/Title: .*/Title: "【转播】kamito"/' "$BASE_DIR/TW/config.yaml"

    # Update YT config
    sed -i 's/ChannelId: .*/ChannelId: UCgYCMluaLpERsyNXlPOvBtA/' "$BASE_DIR/YT/config.yaml"
    sed -i 's/ChannelName: .*/ChannelName: "kamito"/' "$BASE_DIR/YT/config.yaml"
    sed -i 's/Title: .*/Title: "【转播】kamito"/' "$BASE_DIR/YT/config.yaml"
    echo "All configurations updated to kamito."
    display_current_config "all"

    manage_service_after_change "YT"
    manage_service_after_change "TW"

    exit 0
}

# Check for --kamito option
if [ "$1" == "--kamito" ]; then
    update_kamito
    display_current_config
fi

# Function to select area ID
select_area_id() {
    echo "选择分区 ID:"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    echo " 1) 86: 英雄联盟          2) 329: 无畏契约"
    echo " 3) 240: APEX英雄         4) 87: 守望先锋"
    echo " 5) 530: 萌宅领域         6) 235: 其他单机"
    echo " 7) 107: 其他网游         8) 646: UP主日常"
    echo " 9) 102: 最终幻想14      10) 433: 格斗游戏"
    echo "11) 216: 我的世界        12) 927: DeadLock"
    echo " 0) 自定义"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    read -p "请选择分区 ID (0-12): " area_choice

    case $area_choice in
    1) areaid=86 ;;
    2) areaid=329 ;;
    3) areaid=240 ;;
    4) areaid=87 ;;
    5) areaid=530 ;;
    6) areaid=235 ;;
    7) areaid=107 ;;
    8) areaid=646 ;;
    9) areaid=102 ;;
    10) areaid=433 ;;
    11) areaid=216 ;;
    12) areaid=927 ;;
    0) read -p "请输入自定义分区 ID: " areaid ;;
    *)
        echo "无效选择，请重试。"
        return 1
        ;;
    esac
    echo "已选择分区 ID: $areaid"
    return 0
}

# Function to select channel ID
select_channel_id() {
    echo "Select Channel ID:"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    echo " 1) kamito            2) 紫宮るな         3) Narin Mikure"
    echo " 4) 藍沢エマ          5) 八雲べに         6) 兎咲ミミ"
    echo " 7) 英リサ            8) 一ノ瀬うるは     9) 橘ひなの"
    echo "10) 胡桃のあ         11) 猫汰つな        12) 花芽なずな"
    echo "13) 花芽すみれ       14) 獅子堂あかり    15) 紡木こかげ"
    echo "16) 神成きゅぴ       17) 夜絆ニウ        18) 天帝フォルテ"
    echo " 0) Custom"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    read -p "Select Channel ID (0-18): " ch_choice

    case $ch_choice in
    1) chid="UCgYCMluaLpERsyNXlPOvBtA" ;;
    2) chid="UCD5W21JqNMv_tV9nfjvF9sw" ;;
    3) chid="UCKSpM183c85d5V2cW5qaUjA" ;;
    4) chid="UCPkKpOHxEDcwmUAnRpIu-Ng" ;;
    5) chid="UCjXBuHmWkieBApgBhDuJMMQ" ;;
    6) chid="UCnvVG9RbOW3J6Ifqo-zKLiw" ;;
    7) chid="UCurEA8YoqFwimJcAuSHU0MQ" ;;
    8) chid="UC5LyYg6cCA4yHEYvtUsir3g" ;;
    9) chid="UCvUc0m317LWTTPZoBQV479A" ;;
    10) chid="UCIcAj6WkJ8vZ7DeJVgmeqKw" ;;
    11) chid="UCIjdfjcSaEgdjwbgjxC3ZWg" ;;
    12) chid="UCiMG6VdScBabPhJ1ZtaVmbw" ;;
    13) chid="UCyLGcqYs7RsBb3L0SJfzGYA" ;;
    14) chid="UCWRPqA0ehhWV4Hnp27PJCkQ" ;;
    15) chid="UC-WX1CXssCtCtc2TNIRnJzg" ;;
    16) chid="UCMp55EbT_ZlqiMS3lCj01BQ" ;;
    17) chid="UCZmUoMwjyuQ59sk5_7Tx07A" ;;
    18) chid="UC8hwewh9svh92E1gXvgVazg" ;;
    0) read -p "Enter custom Channel ID: " chid ;;
    *)
        echo "Invalid choice. Please try again."
        return 1
        ;;
    esac
    echo "Selected Channel ID: $chid"
    channel_name=$(get_channel_name "$chid")
    new_title="【转播】$channel_name"
    echo "New Title: $new_title"

    # Update the ChannelName in the config file

    return 0
}

# Function to select Twitch ID
select_twitch_id() {
    echo "Select Twitch Channel:"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    echo " 1) kamito              6) とおこ"
    echo " 2) 橘ひなの            7) 天帝フォルテ"
    echo " 3) 花芽すみれ          8) 獅子堂あかり"
    echo " 4) 夢野あかり          9) 夜よいち"
    echo " 5) 白波らむね         10) 甘城なつき"
    echo "11) 胡桃のあ            0) Custom"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    read -p "Select Channel (0-11): " twitch_choice

    case $twitch_choice in
    1)
        channel_id="kamito_jp"
        channel_name="kamito"
        ;;
    2)
        channel_id="hinanotachiba7"
        channel_name="橘ひなの"
        ;;
    3)
        channel_id="kagasumire"
        channel_name="花芽すみれ"
        ;;
    4)
        channel_id="akarindao"
        channel_name="夢野あかり"
        ;;
    5)
        channel_id="ramuneshiranami"
        channel_name="白波らむね"
        ;;
    6)
        channel_id="urs_toko"
        channel_name="とおこ"
        ;;
    7)
        channel_id="tentei_forte"
        channel_name="天帝フォルテ"
        ;;
    8)
        channel_id="shishidoakari"
        channel_name="獅子堂あかり"
        ;;
    9)
        channel_id="yoichi_0v0"
        channel_name="夜よいち"
        ;;
    10)
        channel_id="nacho_dayo"
        channel_name="甘城なつき"
        ;;
    11)
        channel_id="963noah"
        channel_name="胡桃のあ"
        ;;
    0)
        read -p "Enter Twitch ID: " channel_id
        read -p "Enter Channel Name: " channel_name
        ;;
    *)
        echo "Invalid choice. Please try again."
        return 1
        ;;
    esac
    echo "Selected Channel: $channel_name"
    new_title="【转播】$channel_name"
    echo "New Title: $new_title"
    return 0
}

# Function to manage service after config change
manage_service_after_change() {
    local service=$1
    if is_service_running "$service"; then
        read -p "bilistream-$service is running. Restart it? (y/n): " restart_choice
        if [[ $restart_choice =~ ^[Yy]$ ]]; then
            # After stopping bilistream, run bili_stop_live
            echo -e "${YELLOW}If need to stop bili_stop_live, enter (Y/n):${RESET}"
            read stop_choice
            stop_choice=${stop_choice:-Y} # Default to 'Y' if input is empty
            if [[ $stop_choice =~ ^[Yy]$ ]]; then
                ./bili_stop_live
            fi
            restart_service "$service"
        else
            echo "bilistream-$service was not restarted."
        fi
    else
        read -p "bilistream-$service is not running. Start it? (y/N): " start_choice
        start_choice=${start_choice:-N} # Default to 'N' if input is empty
        if [[ $start_choice =~ ^[Yy]$ ]]; then
            start_service "$service"
        else
            echo "bilistream-$service was not started."
        fi
    fi
}

# Function to set up log rotation
setup_log_rotation() {
    if ! pm2 list | grep -q "pm2-logrotate"; then
        pm2 install pm2-logrotate
        pm2 set pm2-logrotate:max_size 1M
        pm2 set pm2-logrotate:retain 3
        pm2 set pm2-logrotate:compress false
        pm2 set pm2-logrotate:dateFormat YYYY-MM-DD_HH-mm-ss
        pm2 set pm2-logrotate:workerInterval 300 # 5 minutes
        pm2 set pm2-logrotate:rotateInterval '0 0 * * *'
        echo "Log rotation set up for PM2"
    else
        echo "Log rotation is already set up for PM2"
    fi
}

# Function to disable log rotation
disable_log_rotation() {
    if pm2 list | grep -q "pm2-logrotate"; then
        pm2 uninstall pm2-logrotate
        echo "PM2 log rotation has been disabled."
    else
        echo "PM2 log rotation is not currently installed."
    fi
}

# Call the function to set up log rotation
setup_log_rotation

# Add this function to map Channel IDs to names
get_channel_name() {
    local channel_id=$1
    case $channel_id in
    "UCgYCMluaLpERsyNXlPOvBtA") echo "kamito" ;;
    "UCD5W21JqNMv_tV9nfjvF9sw") echo "紫宮るな" ;;
    "UCKSpM183c85d5V2cW5qaUjA") echo "Narin Mikure" ;;
    "UCPkKpOHxEDcwmUAnRpIu-Ng") echo "藍沢エマ" ;;
    "UCjXBuHmWkieBApgBhDuJMMQ") echo "八雲べに" ;;
    "UCnvVG9RbOW3J6Ifqo-zKLiw") echo "兎咲ミミ" ;;
    "UCurEA8YoqFwimJcAuSHU0MQ") echo "英リサ" ;;
    "UC5LyYg6cCA4yHEYvtUsir3g") echo "一ノ瀬うるは" ;;
    "UCvUc0m317LWTTPZoBQV479A") echo "橘ひなの" ;;
    "UCIcAj6WkJ8vZ7DeJVgmeqKw") echo "胡桃のあ" ;;
    "UCIjdfjcSaEgdjwbgjxC3ZWg") echo "猫汰つな" ;;
    "UCiMG6VdScBabPhJ1ZtaVmbw") echo "花芽なずな" ;;
    "UCyLGcqYs7RsBb3L0SJfzGYA") echo "花芽すみれ" ;;
    "UCWRPqA0ehhWV4Hnp27PJCkQ") echo "獅子堂あかり" ;;
    "UC-WX1CXssCtCtc2TNIRnJzg") echo "紡木こかげ" ;;
    "UCMp55EbT_ZlqiMS3lCj01BQ") echo "神成きゅぴ" ;;
    "UCZmUoMwjyuQ59sk5_7Tx07A") echo "夜絆ニウ" ;;
    "UC8hwewh9svh92E1gXvgVazg") echo "天帝フォルテ" ;;
    *) echo "Unknown Channel" ;;
    esac
}

# Function to map Area IDs to names
get_area_name() {
    local area_id=$1
    case $area_id in
    86) echo "英雄联盟" ;;
    329) echo "无畏契约" ;;
    240) echo "APEX英雄" ;;
    87) echo "守望先锋" ;;
    530) echo "萌宅领域" ;;
    235) echo "其他单机" ;;
    107) echo "其他网游" ;;
    646) echo "UP主日常" ;;
    102) echo "最终幻想14" ;;
    433) echo "格斗游戏" ;;
    216) echo "我的世界" ;;
    927) echo "DeadLock" ;;
    *) echo "未知分区 (ID: $area_id)" ;;
    esac
}

# Update the display_current_config function
display_current_config() {
    echo
    echo -e "${GREEN}┌─ Bilistream Configuration${RESET}"

    if [ "$1" = "YT" ] || [ "$1" = "all" ]; then
        echo -e "${GREEN}├─── YouTube (YT)${RESET}"
        local yt_area_id=$(grep 'Area_v2:' "$BASE_DIR/YT/config.yaml" | awk '{print $2}')
        local yt_area_name=$(get_area_name "$yt_area_id")
        local yt_channel_id=$(awk '/Youtube:/{flag=1; next} /ChannelId:/ && flag {print $2; exit}' "$BASE_DIR/YT/config.yaml")
        local yt_channel_name=$(awk '/Youtube:/{flag=1; next} /ChannelName:/ && flag {gsub(/"/, ""); print $2; exit}' "$BASE_DIR/YT/config.yaml")
        local yt_title=$(grep 'Title:' "$BASE_DIR/YT/config.yaml" | sed 's/Title: //' | sed 's/"//g')
        echo -e "${GREEN}│    ├─ Area: ${RESET}$yt_area_name (ID: $yt_area_id)"
        echo -e "${GREEN}│    ├─ Channel ID: ${RESET}$yt_channel_id"
        echo -e "${GREEN}│    ├─ Channel Name: ${RESET}$yt_channel_name"
        echo -e "${GREEN}│    └─ Title: ${RESET}$yt_title"
    fi

    if [ "$1" = "TW" ] || [ "$1" = "all" ]; then
        echo -e "${GREEN}├─── Twitch (TW)${RESET}"
        local tw_area_id=$(grep 'Area_v2:' "$BASE_DIR/TW/config.yaml" | awk '{print $2}')
        local tw_area_name=$(get_area_name "$tw_area_id")
        local tw_channel_id=$(awk '/Twitch:/{flag=1; next} /ChannelId:/ && flag {print $2; exit}' "$BASE_DIR/TW/config.yaml")
        local tw_channel_name=$(awk '/Twitch:/{flag=1; next} /ChannelName:/ && flag {gsub(/"/, ""); print $2; exit}' "$BASE_DIR/TW/config.yaml")
        local tw_title=$(grep 'Title:' "$BASE_DIR/TW/config.yaml" | sed 's/Title: //' | sed 's/"//g')
        echo -e "${GREEN}│    ├─ Area: ${RESET}$tw_area_name (ID: $tw_area_id)"
        echo -e "${GREEN}│    ├─ Channel ID: ${RESET}$tw_channel_id"
        echo -e "${GREEN}│    ├─ Channel Name: ${RESET}$tw_channel_name"
        echo -e "${GREEN}│    └─ Title: ${RESET}$tw_title"
    fi

    echo
    read -p "Press Enter to Continue..."
    echo
}

# Function to stop live stream
stop_live_stream() {
    echo "Stopping live stream..."
    ./bili_stop_live -c ./YT/config.yaml
    echo "Stopped bili_stop_live."
}

manage_danmaku_service() {
    if pm2 list | grep -q "danmaku"; then
        echo "danmaku service is currently running."
        echo "1. Restart service"
        echo "2. Stop service"
        echo "3. Delete service"
        echo "4. Do nothing"
        read -p "Enter your choice (1/2/3/4): " action

        case $action in
        1) pm2 restart danmaku ;;
        2) pm2 stop danmaku ;;
        3) pm2 delete danmaku ;;
        4) echo "No action taken." ;;
        *) echo "Invalid choice. No action taken." ;;
        esac
    else
        echo "danmaku service is not running."
        read -p "Start danmaku service? (y/N): " action
        if [[ $action =~ ^[Yy]$ ]]; then
            pm2 start danmaku.sh --name danmaku
        else
            echo "danmaku service was not started."
        fi
    fi
}
# Main menu
while true; do
    # Display the menu
    echo -e "${PINK}┌─────────────────────────────────────┐${RESET}"
    echo -e "${PINK}│       ${RESET}Bilistream Manager            ${PINK}│${RESET}"
    echo -e "${PINK}├─────────────────────────────────────┤${RESET}"
    echo -e "${PINK}│ ${RESET}1. Change Area ID                   ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}2. Change Channel ID                ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}3. Change Twitch ID                 ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}4. Update SESSDATA and bili_jct     ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}5. Quick setup for kamito           ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}6. Manage PM2 Services              ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}7. Disable log rotation             ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}8. Display current configuration    ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}9. Stop Bili Live                   ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}10. Change Live Title               ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}11. Manage Danmaku Service          ${PINK}│${RESET}"
    echo -e "${PINK}│                                     │${RESET}"
    echo -e "${PINK}│ ${RESET}Enter any other key to exit         ${PINK}│${RESET}"
    echo -e "${PINK}└─────────────────────────────────────┘${RESET}"
    read -p "Enter your choice: " main_choice

    case $main_choice in
    1) # Change Area ID
        select_area_id
        if [ $? -eq 0 ]; then
            echo "┌─────────────────────────────────────┐"
            echo "│  Select config to update (Area ID)  │"
            echo "├─────────────────────────────────────┤"
            echo "│ 1. YouTube (YT)                     │"
            echo "│ 2. Twitch (TW)                      │"
            echo "│ 3. Both                             │"
            echo "│ 4. None                             │"
            echo "└─────────────────────────────────────┘"
            read -p "Enter your choice (1/2/3/4): " area_config_choice

            case $area_config_choice in
            1)
                sed -i "s|Area_v2: .*|Area_v2: ${areaid}|" "$BASE_DIR/YT/config.yaml"
                manage_service_after_change "YT"
                display_current_config "YT"
                ;;
            2)
                sed -i "s|Area_v2: .*|Area_v2: ${areaid}|" "$BASE_DIR/TW/config.yaml"
                manage_service_after_change "TW"
                display_current_config "TW"
                ;;
            3)
                sed -i "s|Area_v2: .*|Area_v2: ${areaid}|" "$BASE_DIR"/*/config.yaml
                manage_service_after_change "YT"
                manage_service_after_change "TW"
                display_current_config "all"
                ;;
            4)
                echo "No changes made."
                ;;
            *)
                echo "Invalid choice. No changes made."
                ;;
            esac
        fi
        ;;
    2) # Change Channel ID
        select_channel_id
        if [ $? -eq 0 ]; then
            sed -i "/Youtube:/,/Twitch:/ s|ChannelId: .*|ChannelId: ${chid}|" "$BASE_DIR/YT/config.yaml"
            sed -i "s|ChannelName: .*|ChannelName: \"${channel_name}\"|" "$BASE_DIR/YT/config.yaml"
            sed -i "s|Title: .*|Title: \"${new_title}\"|" "$BASE_DIR/YT/config.yaml"
            echo "YouTube Channel ID, Channel Name, and Title updated in YT/config.yaml."
            manage_service_after_change "YT"
            display_current_config "YT"
        fi
        ;;
    3) # Change Twitch ID
        select_twitch_id
        if [ $? -eq 0 ]; then
            sed -i "/Twitch:/,$ s|ChannelId: .*|ChannelId: ${channel_id}|" "$BASE_DIR/TW/config.yaml"
            sed -i "/Twitch:/,$ s|ChannelName: .*|ChannelName: \"${channel_name}\"|" "$BASE_DIR/TW/config.yaml"
            sed -i "s|Title: .*|Title: \"${new_title}\"|" "$BASE_DIR/TW/config.yaml"
            echo "Twitch ID, Channel Name, and Title updated in TW/config.yaml."
            manage_service_after_change "TW"
            display_current_config "TW"
        fi
        ;;
    4) # Update SESSDATA and bili_jct
        read -p "Enter the new SESSDATA: " new_sessdata
        read -p "Enter the new bili_jct: " new_bili_jct
        sed -i "s|SESSDATA: .*|SESSDATA: ${new_sessdata}|" "$BASE_DIR"/*/config.yaml
        sed -i "s|bili_jct: .*|bili_jct: ${new_bili_jct}|" "$BASE_DIR"/*/config.yaml
        echo "SESSDATA and bili_jct updated in all config files."
        manage_service_after_change "YT"
        manage_service_after_change "TW"
        ;;
    5) # Quick setup for kamito
        update_kamito
        ;;
    6) # Manage PM2 Services
        pm2_output=$(pm2 list | grep "name\|bilistream")
        if [ -z "$pm2_output" ]; then
            echo "No services are running."
        else
            echo "┌────┬──────────────────┬─────────────┬─────────┬─────────┬──────────┬────────┬──────┬───────────┬──────────┬──────────┬──────────┬──────────┐"

            echo "$pm2_output"
            echo "└────┴──────────────────┴─────────────┴─────────┴─────────┴──────────┴────────┴──────┴───────────┴──────────┴──────────┴──────────┴──────────┘"
        fi
        echo "┌─────────────────────────────────────┐"
        echo "│    Select service to manage         │"
        echo "├─────────────────────────────────────┤"
        echo "│ 1. YouTube (YT)                     │"
        echo "│ 2. Twitch (TW)                      │"
        echo "│ 3. Both Services                    │"
        echo "│ 4. None                             │"
        echo "└─────────────────────────────────────┘"
        read -p "Enter your choice (1/2/3/4): " service_choice

        case $service_choice in
        1) manage_pm2_service "YT" ;;
        2) manage_pm2_service "TW" ;;
        3) manage_all_pm2_services ;;
        4) echo "No action taken." ;;
        *) echo "Invalid choice. No action taken." ;;
        esac

        ;;
    7) # Disable log rotation
        disable_log_rotation
        ;;
    8) # Display current configuration
        display_current_config "all"
        ;;
    9) # Stop live stream
        stop_live_stream
        ;;
    10) # Change live title
        echo "┌─────────────────────────────────────┐"
        echo "│    Select title to change           │"
        echo "├─────────────────────────────────────┤"
        echo "│ 1. YouTube (YT)                     │"
        echo "│ 2. Twitch (TW)                      │"
        echo "│ 3. None                             │"
        echo "└─────────────────────────────────────┘"
        read -p "Enter your choice (1/2/3): " title_choice
        case $title_choice in
        1)
            ./bili_change_live_title -c ./YT/config.yaml
            ;;
        2)
            ./bili_change_live_title -c ./TW/config.yaml
            ;;
        3)
            echo "Remote live title not changed."
            ;;
        *)
            echo "Invalid choice. No changes made."
            ;;
        esac
        ;;
    11) # Manage Danmaku Service
        manage_danmaku_service
        ;;
    *)
        echo "Exiting Bilistream Manager. Goodbye!"
        exit 0
        ;;
    esac
done
