#!/usr/bin/env bash
set -euo pipefail

# Обновлённый файл не заменяет код уже работающего процесса. Перед установкой
# завершаем только ScreenInk, чтобы следующая клавиша Print запустила новый бинарник.
if pgrep -x screenink >/dev/null; then
  pkill -x screenink
fi

install -Dm755 target/release/screenink "$HOME/.local/bin/screenink"
if [[ ! -f "$HOME/.config/screenink/config.toml" ]]; then
  install -Dm644 config.example.toml "$HOME/.config/screenink/config.toml"
fi

echo "ScreenInk установлен в ~/.local/bin/screenink"
echo "Далее выполните команды из README, чтобы назначить Print Screen."
