#!/bin/bash

if [ $# -ne 1 ]; then
    echo "Usage: $0 <compile_commands.json>"
    exit 1
fi

# 读取 JSON 文件内容
json=$(cat $1)

# 解析 JSON，并按目录分组
directories=$(echo "$json" | jq -r '.[].directory' | sort -u)

for dir in $directories; do
    # 根据目录创建对应的 JSON 文件
    filtered_json=$(echo "$json" | jq -r --arg dir "$dir" 'map(select(.directory == $dir))')
    dir_name=$(basename "$dir")
    echo $dir_name
    echo "$filtered_json" > "compile_commands_$dir_name.json"
done
