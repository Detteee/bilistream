#!/bin/bash

# Set BASE_DIR to the bilistream and stream_manager.sh working directory
BASE_DIR=$(pwd)

# Define colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
PINK='\033[38;5;218m'
RESET='\033[0m'

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
    0)
        read -p "请输入自定义分区 ID: " areaid
        ;;
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
    echo "Select YouTube Channel:"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    echo " 1) kamito               8) 橘ひなの            15) 神成きゅぴ"
    echo " 2) 紫宮るな             9) 胡桃のあ             16) 夜絆ニウ"
    echo " 3) Narin Mikure        10) 猫汰つな            17) 天帝フォルテ"
    echo " 4) 藍沢エマ           11) 花芽なずな             18) 千燈ゆうひ"
    echo " 5) 八雲べに            12) 花芽すみれ            19) 一ノ瀬うるは"
    echo " 6) 兎咲ミミ            13) 獅子堂あかり           20) 空澄セナ"
    echo " 7) 英リサ              14) 紡木こかげ            0) Custom"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    read -p "Select Channel (0-20): " yt_choice

    case $yt_choice in
    1)
        chid="UCgYCMluaLpERsyNXlPOvBtA"
        channel_name="kamito"
        ;;
    2)
        chid="UCD5W21JqNMv_tV9nfjvF9sw"
        channel_name="紫宮るな"
        ;;
    3)
        chid="UCKSpM183c85d5V2cW5qaUjA"
        channel_name="Narin_Mikure"
        ;;
    4)
        chid="UCPkKpOHxEDcwmUAnRpIu-Ng"
        channel_name="藍沢エマ"
        ;;
    5)
        chid="UCjXBuHmWkieBApgBhDuJMMQ"
        channel_name="八雲べに"
        ;;
    6)
        chid="UCnvVG9RbOW3J6Ifqo-zKLiw"
        channel_name="兎咲ミミ"
        ;;
    7)
        chid="UCurEA8YoqFwimJcAuSHU0MQ"
        channel_name="英リサ"
        ;;
    8)
        chid="UCvUc0m317LWTTPZoBQV479A"
        channel_name="橘ひなの"
        ;;
    9)
        chid="UCIcAj6WkJ8vZ7DeJVgmeqKw"
        channel_name="胡桃のあ"
        ;;
    10)
        chid="UCIjdfjcSaEgdjwbgjxC3ZWg"
        channel_name="猫汰つな"
        ;;
    11)
        chid="UCiMG6VdScBabPhJ1ZtaVmbw"
        channel_name="花芽なずな"
        ;;
    12)
        chid="UCyLGcqYs7RsBb3L0SJfzGYA"
        channel_name="花芽すみれ"
        ;;
    13)
        chid="UCWRPqA0ehhWV4Hnp27PJCkQ"
        channel_name="獅子堂あかり"
        ;;
    14)
        chid="UC-WX1CXssCtCtc2TNIRnJzg"
        channel_name="紡木こかげ"
        ;;
    15)
        chid="UCMp55EbT_ZlqiMS3lCj01BQ"
        channel_name="神成きゅぴ"
        ;;
    16)
        chid="UCZmUoMwjyuQ59sk5_7Tx07A"
        channel_name="夜絆ニウ"
        ;;
    17)
        chid="UC8hwewh9svh92E1gXvgVazg"
        channel_name="天帝フォルテ"
        ;;
    18)
        chid="UCuDY3ibSP2MFRgf7eo3cojg"
        channel_name="千燈ゆうひ"
        ;;
    19)
        chid="UC5LyYg6cCA4yHEYvtUsir3g"
        channel_name="一ノ瀬うるは"
        ;;
    20)
        chid="UCF_U2GCKHvDz52jWdizppIA"
        channel_name="空澄セナ"
        ;;
    0)
        read -p "Enter custom YouTube Channel ID: " chid
        read -p "Enter custom Channel Name: " channel_name
        read -p "Whether to add this channel to the ./YT/YT_channels.txt? (y/N): " add_choice
        add_choice=${add_choice:-N} # Default to 'N' if input is empty
        if [[ $add_choice =~ ^[Yy]$ ]]; then
            echo "($channel_name) [$chid]" >>"$BASE_DIR/YT/YT_channels.txt"
        fi
        ;;
    *)
        echo "Invalid choice. Exiting."
        return 1
        ;;
    esac

    new_title="【转播】${channel_name}"
    echo "Selected Channel: $channel_name (ID: $chid)"
    echo "New Title: $new_title"
    return 0
}
# Function to select Twitch ID
select_twitch_id() {
    echo "Select Twitch Channel:"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    echo " 1) kamito              6) とおこ              11) 胡桃のあ"
    echo " 2) 橘ひなの            7) 天帝フォルテ        12) 紫宮るな"
    echo " 3) 花芽すみれ          8) 獅子堂あかり        13) 狐白うる"
    echo " 4) 夢野あかり          9) 夜よいち             14) 英リサ"
    echo " 5) 白波らむね         10) 甘城なつき           0) Custom"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    read -p "Select Channel (0-14): " twitch_choice

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
    12)
        channel_id="shinomiya_runa"
        channel_name="紫宮るな"
        ;;
    13)
        channel_id="kohaku_uru"
        channel_name="狐白うる"
        ;;
    14)
        channel_id="lisahanabusa"
        channel_name="英リサ"
        ;;
    0)

        read -p "Enter Twitch ID: " channel_id
        read -p "Enter Channel Name: " channel_name
        read -p "Whether to add this channel to the ./TW/TW_channels.txt? (y/N): " add_choice
        add_choice=${add_choice:-N} # Default to 'N' if input is empty
        if [[ $add_choice =~ ^[Yy]$ ]]; then
            echo "($channel_name) [$channel_id]" >>"$BASE_DIR/TW/TW_channels.txt"
        fi
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

# Function to display the main menu with an additional tmux management option
show_main_menu() {
    echo -e "${PINK}┌─────────────────────────────────────┐${RESET}"
    echo -e "${PINK}│       Bilistream Manager            ${RESET}${PINK}│${RESET}"
    echo -e "${PINK}├─────────────────────────────────────┤${RESET}"
    echo -e "${PINK}│ ${RESET}1. Change Area ID                   ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}2. Change YouTube ID                ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}3. Change Twitch ID                 ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}4. Update SESSDATA and bili_jct     ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}5. Quick setup for kamito           ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}6. Display current config           ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}                                    ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}Enter any other key to exit         ${PINK}│${RESET}"

    echo -e "${PINK}└─────────────────────────────────────┘${RESET}"
    read -p "Enter your choice: " main_choice
}

while true; do
    show_main_menu

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
                display_current_config "YT"
                ;;
            2)
                sed -i "s|Area_v2: .*|Area_v2: ${areaid}|" "$BASE_DIR/TW/config.yaml"
                display_current_config "TW"
                ;;
            3)
                sed -i "s|Area_v2: .*|Area_v2: ${areaid}|" "$BASE_DIR"/*/config.yaml
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
            display_current_config "TW"
        fi
        ;;
    4) # Update SESSDATA and bili_jct
        read -p "Enter the new SESSDATA: " new_sessdata
        read -p "Enter the new bili_jct: " new_bili_jct
        sed -i "s|SESSDATA: .*|SESSDATA: ${new_sessdata}|" "$BASE_DIR"/*/config.yaml
        sed -i "s|bili_jct: .*|bili_jct: ${new_bili_jct}|" "$BASE_DIR"/*/config.yaml
        sed -i "s|\"sessdata\": \".*\"|\"sessdata\": \"${new_sessdata}\"|" "$BASE_DIR/config.json"
        echo "SESSDATA and bili_jct updated in all config files and config.json."
        ;;
    5) # Quick setup for kamito
        update_kamito
        ;;
    6) # Display current config
        display_current_config "all"
        ;;

    *)
        echo "Exiting Bilistream Manager. Goodbye!"
        exit 0
        ;;
    esac
done
