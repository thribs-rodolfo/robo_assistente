#!/usr/bin/env python3
# Ponte HTTP própria para os bots de Telegram (substitui os "canais" do OpenClaw).
# Recebe webhooks do Telegram via nginx (HTTPS) e responde. Sem dependências externas (stdlib).
#
# Filosofia: infra própria, poder mínimo. Cada bot tem secret próprio e lista de quem pode falar.
# Config dos bots (com tokens) fica FORA daqui, em /root/.secrets/ponte-telegram.json (chmod 600).
import json, os, sys, ssl, urllib.request, urllib.parse, logging
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Roteador de provedores (cérebro agnóstico) — fica no mesmo diretório.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from roteador import rotear  # noqa: E402

CONFIG = "/root/.secrets/ponte-telegram.json"
PORTA = 18800
LOG = "/var/log/ponte-telegram.log"
logging.basicConfig(filename=LOG, level=logging.INFO,
                    format="%(asctime)s %(levelname)s %(message)s")

def carregar_config():
    # { "bots": { "<nome>": {"token": "...", "secret": "...",
    #                         "allow_from": [ids] OU "allow_from_arquivo": "/caminho.json"} } }
    with open(CONFIG) as f:
        return json.load(f)

def lista_permitidos(bot):
    ids = bot.get("allow_from")
    if ids:
        return set(str(i) for i in ids)
    arq = bot.get("allow_from_arquivo")
    if arq and os.path.exists(arq):
        try:
            d = json.load(open(arq))
            return set(str(i) for i in d.get("allowFrom", []))
        except Exception as e:
            logging.warning("allowFrom %s ilegível: %s", arq, e)
    return set()  # vazio = ninguém (seguro por padrão)

def telegram(token, metodo, dados):
    url = f"https://api.telegram.org/bot{token}/{metodo}"
    corpo = urllib.parse.urlencode(dados).encode()
    req = urllib.request.Request(url, data=corpo, method="POST")
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.loads(r.read())

def responder(token, chat_id, texto):
    return telegram(token, "sendMessage", {"chat_id": chat_id, "text": texto})

def processar(nome_bot, bot, update):
    """Trata um update. v1: confirma o circuito (eco). Depois liga no agente (depende do refresh do token)."""
    msg = update.get("message") or update.get("edited_message")
    if not msg:
        return
    from_id = str((msg.get("from") or {}).get("id", ""))
    chat_id = (msg.get("chat") or {}).get("id")
    texto = msg.get("text", "")
    permitidos = lista_permitidos(bot)
    if from_id not in permitidos:
        logging.info("[%s] IGNORADO de %s (fora do allowFrom)", nome_bot, from_id)
        return
    logging.info("[%s] msg de %s: %r", nome_bot, from_id, texto[:120])
    # --- v2: resposta real via roteador de provedores (cadeia de fallback) ---
    # O roteador tenta os provedores em ordem; o Ollama local fica na cauda como piso de
    # emergência, então sempre há resposta. Se até ele falhar, mandamos um aviso curto.
    contexto = {"sistema": bot.get("sistema") or
                "Você é um assistente prestativo respondendo no Telegram. Seja conciso e em português do Brasil."}
    try:
        resposta, provedor = rotear(texto, contexto)
        logging.info("[%s] respondido pelo provedor '%s'", nome_bot, provedor)
    except Exception as e:
        logging.error("[%s] roteador falhou em todos os provedores: %s", nome_bot, e)
        resposta = "Desculpe, estou sem conseguir pensar agora (todos os provedores falharam). Tente de novo em instantes."
    try:
        responder(bot["token"], chat_id, resposta)
    except Exception as e:
        logging.error("[%s] falha ao responder: %s", nome_bot, e)

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # silencia o log padrão (usamos logging)
        pass

    def do_POST(self):
        # caminho esperado: /ponte-telegram/<nome_bot>
        partes = self.path.strip("/").split("/")
        if len(partes) != 2 or partes[0] != "ponte-telegram":
            self.send_response(404); self.end_headers(); return
        nome_bot = partes[1]
        try:
            cfg = carregar_config()
        except Exception as e:
            logging.error("config ilegível: %s", e)
            self.send_response(500); self.end_headers(); return
        bot = cfg.get("bots", {}).get(nome_bot)
        if not bot:
            self.send_response(404); self.end_headers(); return
        # valida o secret do webhook (Telegram envia no header)
        recebido = self.headers.get("X-Telegram-Bot-Api-Secret-Token", "")
        if recebido != bot.get("secret", ""):
            logging.warning("[%s] secret inválido", nome_bot)
            self.send_response(403); self.end_headers(); return
        tam = int(self.headers.get("Content-Length", 0) or 0)
        corpo = self.rfile.read(tam) if tam else b"{}"
        # responde 200 imediatamente ao Telegram (boa prática); processa em seguida
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.end_headers(); self.wfile.write(b'{"ok":true}')
        try:
            processar(nome_bot, bot, json.loads(corpo or b"{}"))
        except Exception as e:
            logging.error("[%s] erro ao processar: %s", nome_bot, e)

    def do_GET(self):
        # healthcheck simples
        if self.path == "/ponte-telegram/saude":
            self.send_response(200); self.send_header("Content-Type", "text/plain")
            self.end_headers(); self.wfile.write(b"ok")
        else:
            self.send_response(404); self.end_headers()

if __name__ == "__main__":
    logging.info("ponte-telegram iniciando em 127.0.0.1:%d", PORTA)
    ThreadingHTTPServer(("127.0.0.1", PORTA), Handler).serve_forever()
