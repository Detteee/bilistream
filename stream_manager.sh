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
    # Update Twitch section
    sed -i '/Twitch:/,$ s|ChannelId: .*|ChannelId: kamito_jp|' "$BASE_DIR/config.yaml"
    sed -i '/Twitch:/,$ s|ChannelName: .*|ChannelName: "Kamito"|' "$BASE_DIR/config.yaml"

    # Update YouTube section
    sed -i '/Youtube:/,/Twitch:/ s|ChannelId: .*|ChannelId: UCgYCMluaLpERsyNXlPOvBtA|' "$BASE_DIR/config.yaml"
    sed -i '/Youtube:/,/Twitch:/ s|ChannelName: .*|ChannelName: "Kamito"|' "$BASE_DIR/config.yaml"

    echo "All configurations updated to Kamito."
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
    echo "13) 236: 主机游戏        14) 321: 原神"
    echo "15) 407: 游戏王          16) 694: 斯普拉遁3"
    echo "17) 252: 逃离塔科夫      18) 318: 使命召唤:战区"
    echo "19) 555: 艾尔登法环      20) 578: 怪物猎人"
    echo "21) 308: 塞尔达传说      22) 878: 三角洲行动"
    echo "23) 795: Dark and Darker 24) 858: 致命公司"
    echo " 0) 自定义"
    echo -e "${GREEN}──────────────────────────────────────────────────────────${RESET}"
    read -p "请选择分区 ID (0-24): " area_choice

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
    13) areaid=236 ;;
    14) areaid=321 ;;
    15) areaid=407 ;;
    16) areaid=694 ;;
    17) areaid=252 ;;
    18) areaid=318 ;;
    19) areaid=555 ;;
    20) areaid=578 ;;
    21) areaid=308 ;;
    22) areaid=878 ;;
    23) areaid=795 ;;
    24) areaid=858 ;;
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

update_riot_api_key() {
    echo "从https://developer.riotgames.com/获取API Key"
    read -p "请输入Riot API Key: " riot_api_key
    if [ ! -z "$riot_api_key" ]; then
        sed -i "s/RiotApiKey: .*/RiotApiKey: $riot_api_key/" "$BASE_DIR/config.yaml"
        echo "Riot API Key 已更新。"
    else
        echo "Riot API Key 为空，跳过更新。"
    fi
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
    236) echo "主机游戏" ;;
    321) echo "原神" ;;
    407) echo "游戏王：决斗链接" ;;
    694) echo "斯普拉遁3" ;;
    252) echo "逃离塔科夫" ;;
    318) echo "使命召唤:战区" ;;
    555) echo "艾尔登法环" ;;
    578) echo "怪物猎人" ;;
    308) echo "塞尔达传说" ;;
    878) echo "三角洲行动" ;;
    795) echo "Dark and Darker" ;;
    858) echo "致命公司" ;;
    *) echo "未知分区 (ID: $area_id)" ;;
    esac
}

# Function to select channel ID
select_channel_id() {
    echo "Select YouTube Channel:"
    echo -e "${GREEN}───────────────────────────────────────────────────────────────${RESET}"
    echo " 1) Kamito              2) 花芽なずな           3) 花芽すみれ"
    echo " 4) 小雀とと            5) 一ノ瀬うるは         6) 胡桃のあ"
    echo " 7) 兎咲ミミ            8) 空澄セナ             9) 橘ひなの"
    echo "10) 英リサ             11) 如月れん            12) 神成きゅぴ"
    echo "13) 八雲べに           14) 藍沢エマ            15) 紫宮るな"
    echo "16) 猫汰つな           17) 小森めと            18) 白波らむね"
    echo "19) 夢野あかり         20) 夜乃くろむ          21) 紡木こかげ"
    echo "22) 千燈ゆうひ         23) 蝶屋はなび          24) 甘結もか"
    echo "25) Narin Mikure       26) 獅子堂あかり        27) 天帝フォルテ"
    echo "28) 夜絆ニウ           29) 白那しずく          30) 絲依とい"
    echo "31) ぶいすぽっ!【公式】"
    echo " 0) Custom"
    echo -e "${GREEN}───────────────────────────────────────────────────────────────${RESET}"
    read -p "Select Channel (0-31): " yt_choice

    case $yt_choice in
    1)
        chid="UCgYCMluaLpERsyNXlPOvBtA"
        channel_name="Kamito"
        ;;
    2)
        chid="UCiMG6VdScBabPhJ1ZtaVmbw"
        channel_name="花芽なずな"
        ;;
    3)
        chid="UCyLGcqYs7RsBb3L0SJfzGYA"
        channel_name="花芽すみれ"
        ;;
    4)
        chid="UCgTzsBI0DIRopMylJEDqnog"
        channel_name="小雀とと"
        ;;
    5)
        chid="UC5LyYg6cCA4yHEYvtUsir3g"
        channel_name="一ノ瀬うるは"
        ;;
    6)
        chid="UCIcAj6WkJ8vZ7DeJVgmeqKw"
        channel_name="胡桃のあ"
        ;;
    7)
        chid="UCnvVG9RbOW3J6Ifqo-zKLiw"
        channel_name="兎咲ミミ"
        ;;
    8)
        chid="UCF_U2GCKHvDz52jWdizppIA"
        channel_name="空澄セナ"
        ;;
    9)
        chid="UCvUc0m317LWTTPZoBQV479A"
        channel_name="橘ひなの"
        ;;
    10)
        chid="UCurEA8YoqFwimJcAuSHU0MQ"
        channel_name="英リサ"
        ;;
    11)
        chid="UCGWa1dMU_sDCaRQjdabsVgg"
        channel_name="如月れん"
        ;;
    12)
        chid="UCMp55EbT_ZlqiMS3lCj01BQ"
        channel_name="神成きゅぴ"
        ;;
    13)
        chid="UCjXBuHmWkieBApgBhDuJMMQ"
        channel_name="八雲べに"
        ;;
    14)
        chid="UCPkKpOHxEDcwmUAnRpIu-Ng"
        channel_name="藍沢エマ"
        ;;
    15)
        chid="UCD5W21JqNMv_tV9nfjvF9sw"
        channel_name="紫宮るな"
        ;;
    16)
        chid="UCIjdfjcSaEgdjwbgjxC3ZWg"
        channel_name="猫汰つな"
        ;;
    17)
        chid="UCzUNASdzI4PV5SlqtYwAkKQ"
        channel_name="小森めと"
        ;;
    18)
        chid="UC61OwuYOVuKkpKnid-43Twg"
        channel_name="白波らむね"
        ;;
    19)
        chid="UCS5l_Y0oMVTjEos2LuyeSZQ"
        channel_name="夢野あかり"
        ;;
    20)
        chid="UCX4WL24YEOUYd7qDsFSLDOw"
        channel_name="夜乃くろむ"
        ;;
    21)
        chid="UC-WX1CXssCtCtc2TNIRnJzg"
        channel_name="紡木こかげ"
        ;;
    22)
        chid="UCuDY3ibSP2MFRgf7eo3cojg"
        channel_name="千燈ゆうひ"
        ;;
    23)
        chid="UCL9hJsdk9eQa0IlWbFB2oRg"
        channel_name="蝶屋はなび"
        ;;
    24)
        chid="UC8vKBjGY2HVfbW9GAmgikWw"
        channel_name="甘結もか"
        ;;
    25)
        chid="UCKSpM183c85d5V2cW5qaUjA"
        channel_name="Narin"
        ;;
    26)
        chid="UCWRPqA0ehhWV4Hnp27PJCkQ"
        channel_name="獅子堂あかり"
        ;;
    27)
        chid="UC8hwewh9svh92E1gXvgVazg"
        channel_name="天帝フォルテ"
        ;;
    28)
        chid="UCZmUoMwjyuQ59sk5_7Tx07A"
        channel_name="夜絆ニウ"
        ;;
    29)
        chid="UCAHQGIKolfBfoeXXMY79SBA"
        channel_name="白那しずく"
        ;;
    30)
        chid="UCZrYHIPsKhYAXqOls2kQQNQ"
        channel_name="絲依とい"
        ;;
    31)
        chid="UCuI5XaO-6VkOEhHao6ij7JA"
        channel_name="ぶいすぽっ!【公式】"
        ;;
    0)
        read -p "Enter custom YouTube Channel ID: " chid
        read -p "Enter custom Channel Name: " channel_name
        read -p "Whether to add/update this channel in channels.json? (y/N): " add_choice
        add_choice=${add_choice:-N} # Default to 'N' if input is empty
        if [[ $add_choice =~ ^[Yy]$ ]]; then
            # Check if channel already exists
            if jq -e ".channels[] | select(.name == \"$channel_name\")" channels.json > /dev/null; then
                # Update existing channel's YouTube platform
                temp_file=$(mktemp)
                jq "(.channels[] | select(.name == \"$channel_name\").platforms.youtube) |= \"$chid\"" channels.json > "$temp_file"
                mv "$temp_file" channels.json
                echo "Updated YouTube ID for existing channel: $channel_name"
            else
                # Create new channel entry
                new_channel="{
                  \"name\": \"$channel_name\",
                  \"platforms\": {
                    \"youtube\": \"$chid\"
                  }
                }"
                
                # Add new channel to JSON file
                temp_file=$(mktemp)
                jq ".channels += [$new_channel]" channels.json > "$temp_file"
                mv "$temp_file" channels.json
                echo "Added new channel: $channel_name"
            fi
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
    echo -e "${GREEN}─────────────────────────────────────────────────────────────${RESET}"
    echo " 1) Kamito              2) 花芽すみれ           3) 英リサ"
    echo " 4) 如月れん            5) 胡桃のあ             6) 橘ひなの"
    echo " 7) 空澄セナ            8) 藍沢エマ             9) 紫宮るな"
    echo "10) 夢野あかり         11) 白波らむね          12) 千燈ゆうひ"
    echo "13) Narin             14) とおこ              15) 天帝フォルテ"
    echo "16) 獅子堂あかり       17) 夜よいち            18) Nachoneko"
    echo "19) 狐白うる          20) Anzu_o0             21) まふゆ"
    echo "22) 白那しずく         23) 絲依とい"
    echo " 0) Custom"
    echo -e "${GREEN}─────────────────────────────────────────────────────────────${RESET}"
    read -p "Select Channel (0-23): " twitch_choice

    case $twitch_choice in
    1)
        channel_id="kamito_jp"
        channel_name="Kamito"
        ;;
    2)
        channel_id="kagasumire"
        channel_name="花芽すみれ"
        ;;
    3)
        channel_id="lisahanabusa"
        channel_name="英リサ"
        ;;
    4)
        channel_id="ren_kisaragi__"
        channel_name="如月れん"
        ;;
    5)
        channel_id="963noah"
        channel_name="胡桃のあ"
        ;;
    6)
        channel_id="hinanotachiba7"
        channel_name="橘ひなの"
        ;;
    7)
        channel_id="asumisena"
        channel_name="空澄セナ"
        ;;
    8)
        channel_id="emtsmaru"
        channel_name="藍沢エマ"
        ;;
    9)
        channel_id="shinomiya_runa"
        channel_name="紫宮るな"
        ;;
    10)
        channel_id="akarindao"
        channel_name="夢野あかり"
        ;;
    11)
        channel_id="ramuneshiranami"
        channel_name="白波らむね"
        ;;
    12)
        channel_id="sendo_yuuhi"
        channel_name="千燈ゆうひ"
        ;;
    13)
        channel_id="narinmikure"
        channel_name="Narin"
        ;;
    14)
        channel_id="urs_toko"
        channel_name="とおこ"
        ;;
    15)
        channel_id="tentei_forte"
        channel_name="天帝フォルテ"
        ;;
    16)
        channel_id="shishidoakari"
        channel_name="獅子堂あかり"
        ;;
    17)
        channel_id="yoichi_0v0"
        channel_name="夜よいち"
        ;;
    18)
        channel_id="nacho_dayo"
        channel_name="Nachoneko"
        ;;
    19)
        channel_id="kohaku_uru"
        channel_name="狐白うる"
        ;;
    20)
        channel_id="anzu_o0"
        channel_name="Anzu_o0"
        ;;
    21)
        channel_id="mmafu_"
        channel_name="まふゆ"
        ;;
    22)
        channel_id="shirona_shizuku"
        channel_name="白那しずく"
        ;;
    23)
        channel_id="itoitoi_q"
        channel_name="絲依とい"
        ;;
    0)
        read -p "Enter custom Twitch Channel ID: " channel_id
        read -p "Enter custom Channel Name: " channel_name
        read -p "Whether to add/update this channel in channels.json? (y/N): " add_choice
        add_choice=${add_choice:-N} # Default to 'N' if input is empty
        if [[ $add_choice =~ ^[Yy]$ ]]; then
            # Check if channel already exists
            if jq -e ".channels[] | select(.name == \"$channel_name\")" channels.json > /dev/null; then
                # Update existing channel's Twitch platform
                temp_file=$(mktemp)
                jq "(.channels[] | select(.name == \"$channel_name\").platforms.twitch) |= \"$channel_id\"" channels.json > "$temp_file"
                mv "$temp_file" channels.json
                echo "Updated Twitch ID for existing channel: $channel_name"
            else
                # Create new channel entry
                new_channel="{
                  \"name\": \"$channel_name\",
                  \"platforms\": {
                    \"twitch\": \"$channel_id\"
                  }
                }"
                
                # Add new channel to JSON file
                temp_file=$(mktemp)
                jq ".channels += [$new_channel]" channels.json > "$temp_file"
                mv "$temp_file" channels.json
                echo "Added new channel: $channel_name"
            fi
        fi
        ;;
    *)
        echo "Invalid choice. Exiting."
        return 1
        ;;
    esac
    echo "Selected Channel: $channel_name"
    new_title="【转播】$channel_name"
    echo "New Title: $new_title"
    return 0
}

# Update the display_current_config function
display_current_config() {
    echo
    echo -e "${GREEN}┌─ Bilistream Configuration${RESET}"

    if [ "$1" = "YT" ] || [ "$1" = "all" ]; then
        echo -e "${GREEN}├─── YouTube (YT)${RESET}"
        local yt_area_id=$(awk '/^Youtube:/{flag=1} flag&&/^[[:space:]]*Area_v2:/{print $2; exit}' "$BASE_DIR/config.yaml")
        local yt_area_name=$(get_area_name "$yt_area_id")
        local yt_channel_id=$(awk '/^Youtube:/{flag=1} flag&&/^[[:space:]]*ChannelId:/{print $2; exit}' "$BASE_DIR/config.yaml")
        local yt_channel_name=$(awk '/^Youtube:/{flag=1} flag&&/^[[:space:]]*ChannelName:/{gsub(/"/, ""); print $2; exit}' "$BASE_DIR/config.yaml")
        echo -e "${GREEN}│    ├─ Area: ${RESET}$yt_area_name (ID: $yt_area_id)"
        echo -e "${GREEN}│    ├─ Channel ID: ${RESET}$yt_channel_id"
        echo -e "${GREEN}│    └─ Channel Name: ${RESET}$yt_channel_name"
    fi

    if [ "$1" = "TW" ] || [ "$1" = "all" ]; then
        echo -e "${GREEN}├─── Twitch (TW)${RESET}"
        local tw_area_id=$(awk '/^Twitch:/{flag=1} flag&&/^[[:space:]]*Area_v2:/{print $2; exit}' "$BASE_DIR/config.yaml")
        local tw_area_name=$(get_area_name "$tw_area_id")
        local tw_channel_id=$(awk '/^Twitch:/{flag=1} flag&&/^[[:space:]]*ChannelId:/{print $2; exit}' "$BASE_DIR/config.yaml")
        local tw_channel_name=$(awk '/^Twitch:/{flag=1} flag&&/^[[:space:]]*ChannelName:/{gsub(/"/, ""); print $2; exit}' "$BASE_DIR/config.yaml")
        echo -e "${GREEN}│    ├─ Area: ${RESET}$tw_area_name (ID: $tw_area_id)"
        echo -e "${GREEN}│    ├─ Channel ID: ${RESET}$tw_channel_id"
        echo -e "${GREEN}│    └─ Channel Name: ${RESET}$tw_channel_name"
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
    echo -e "${PINK}│ ${RESET}4. Quick setup for kamito           ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}5. Display current config           ${PINK}│${RESET}"
    echo -e "${PINK}│ ${RESET}6. Update Riot API Key              ${PINK}│${RESET}"
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
                sed -i "/Youtube:/,/Twitch:/ s|Area_v2: .*|Area_v2: ${areaid}|" "$BASE_DIR/config.yaml"
                display_current_config "YT"
                ;;
            2)
                sed -i "/Twitch:/,/Youtube:/ s|Area_v2: .*|Area_v2: ${areaid}|" "$BASE_DIR/config.yaml"
                display_current_config "TW"
                ;;
            3)
                sed -i "/Youtube:/,/Twitch:/ s|Area_v2: .*|Area_v2: ${areaid}|" "$BASE_DIR/config.yaml"
                sed -i "/Twitch:/,/Youtube:/ s|Area_v2: .*|Area_v2: ${areaid}|" "$BASE_DIR/config.yaml"
                display_current_config "all"
                ;;
            4)
                echo "No changes made."
                ;;
            *)
                echo "Invalid choice. No changes made."
                ;;
            esac

            if [ "$areaid" -eq 86 ]; then
                update_riot_api_key
            fi  
        fi
        ;;
    2) # Change YouTube Channel ID
        select_channel_id
        if [ $? -eq 0 ]; then
            sed -i "/Youtube:/,/Twitch:/ s|ChannelId: .*|ChannelId: ${chid}|" "$BASE_DIR/config.yaml"
            sed -i "/Youtube:/,/Twitch:/ s|ChannelName: .*|ChannelName: \"${channel_name}\"|" "$BASE_DIR/config.yaml"
            echo "YouTube Channel ID and Channel Name updated in config.yaml."
            display_current_config "YT"
        fi
        ;;
    3) # Change Twitch ID
        select_twitch_id
        if [ $? -eq 0 ]; then
            sed -i "/Twitch:/,/Youtube:/ s|ChannelId: .*|ChannelId: ${channel_id}|" "$BASE_DIR/config.yaml"
            sed -i "/Twitch:/,/Youtube:/ s|ChannelName: .*|ChannelName: \"${channel_name}\"|" "$BASE_DIR/config.yaml"
            echo "Twitch ID and Channel Name updated in config.yaml."
            display_current_config "TW"
        fi
        ;;
    4) # Quick setup for kamito
        update_kamito
        ;;
    5) # Display current config
        display_current_config "all"
        ;;
    6) # Update Riot API Key
        update_riot_api_key
        ;;

    *)
        echo "Exiting Bilistream Manager. Goodbye!"
        exit 0
        ;;
    esac
done
