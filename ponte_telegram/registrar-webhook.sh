#!/bin/bash
# Registra/remove o webhook de um bot no Telegram, apontando p/ a ponte via nginx.
# uso: registrar-webhook.sh <nome_bot>            -> registra
#      registrar-webhook.sh <nome_bot> --remover  -> remove (volta o bot p/ polling/OpenClaw)
set -e
NOME="$1"; CFG=/root/.secrets/ponte-telegram.json
DOMINIO="rodolfo.joelpires.com.br"
TOKEN=$(python3 -c "import json;print(json.load(open('$CFG'))['bots']['$NOME']['token'])")
SECRET=$(python3 -c "import json;print(json.load(open('$CFG'))['bots']['$NOME']['secret'])")
if [ "$2" = "--remover" ]; then
  curl -s "https://api.telegram.org/bot$TOKEN/deleteWebhook" ; echo; exit 0
fi
URL="https://$DOMINIO/ponte-telegram/$NOME"
curl -s -X POST "https://api.telegram.org/bot$TOKEN/setWebhook" \
  -d "url=$URL" -d "secret_token=$SECRET" -d "drop_pending_updates=true"; echo
echo "webhook -> $URL"
curl -s "https://api.telegram.org/bot$TOKEN/getWebhookInfo"; echo
