#!/usr/bin/env python3
# Roteador de provedores — cérebro agnóstico da ponte/agente.
#
# Objetivo: a ponte deixa de chamar `claude --print` direto. Passa a chamar este roteador,
# que tenta os provedores em ordem de fallback. Se um falha (sem chave, 401, 429, timeout,
# erro), passa pro próximo. O Ollama local fica SEMPRE por último: piso de emergência,
# custo zero, nunca deixa o robô mudo.
#
# Interface comum de cada provedor:  responder(mensagem, contexto) -> texto
#
# Sem dependências externas (stdlib). Config/chaves em /root/.secrets/roteador-provedores.json
# (fora do repositório). Telemetria simples: registra qual provedor respondeu.
#
# PASSO 1 (este): esqueleto + provider Ollama local (provado) + provider Claude (`claude --print`).
# Os providers Groq e Gemini ficam declarados mas DESABILITADOS até o passo 2 (chaves válidas).
import json
import os
import subprocess
import urllib.request
import urllib.error
import logging

CAMINHO_CONFIG = "/root/.secrets/roteador-provedores.json"
ARQUIVO_LOG = "/var/log/roteador-provedores.log"

logging.basicConfig(
    filename=ARQUIVO_LOG,
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(message)s",
)


class FalhaProvedor(Exception):
    """Erro recuperável de um provedor: o roteador deve cair pro próximo da cadeia."""
    pass


def carregar_config(caminho=CAMINHO_CONFIG):
    with open(caminho) as arquivo:
        return json.load(arquivo)


# --------------------------------------------------------------------------- #
# Provedores. Cada um implementa responder(mensagem, contexto) -> texto.
# Em qualquer falha recuperável, levanta FalhaProvedor pra acionar o fallback.
# --------------------------------------------------------------------------- #
class Provedor:
    """Classe base. Subclasses implementam responder()."""

    nome = "base"

    def __init__(self, nome, configuracao):
        self.nome = nome
        self.configuracao = configuracao
        self.timeout = configuracao.get("timeout_segundos", 60)

    def disponivel(self):
        """Pré-checagem barata (tem chave? está habilitado?) antes de gastar rede."""
        return self.configuracao.get("habilitado", True)

    def responder(self, mensagem, contexto):
        raise NotImplementedError


class ProvedorOllama(Provedor):
    """Modelo local via API do Ollama. Piso de emergência: lento e fraco, mas sempre vivo."""

    def responder(self, mensagem, contexto):
        url = self.configuracao["url_base"].rstrip("/") + "/api/generate"
        carga = {
            "model": self.configuracao["modelo"],
            "prompt": _montar_prompt(mensagem, contexto),
            "stream": False,
        }
        corpo = json.dumps(carga).encode()
        requisicao = urllib.request.Request(
            url, data=corpo, headers={"Content-Type": "application/json"}, method="POST"
        )
        try:
            with urllib.request.urlopen(requisicao, timeout=self.timeout) as resposta:
                dados = json.loads(resposta.read())
        except (urllib.error.URLError, TimeoutError, OSError) as erro:
            raise FalhaProvedor(f"ollama indisponível: {erro}")
        texto = (dados.get("response") or "").strip()
        if not texto:
            raise FalhaProvedor("ollama devolveu resposta vazia")
        return texto


class ProvedorClaudeCLI(Provedor):
    """Claude via CLI (`claude --print`). Usa o token OAuth já instalado na máquina.

    Importante: este provider NÃO mexe no refresh do token. Quem rotaciona o refresh é só
    o cron de produção. Um `claude --print` que falhe (token caído etc.) apenas levanta
    FalhaProvedor e o roteador cai pro próximo — sem nunca chamar o endpoint de refresh.
    """

    def responder(self, mensagem, contexto):
        comando = [self.configuracao.get("comando", "claude"), "--print"]
        prompt = _montar_prompt(mensagem, contexto)
        try:
            processo = subprocess.run(
                comando,
                input=prompt,
                capture_output=True,
                text=True,
                timeout=self.timeout,
            )
        except FileNotFoundError:
            raise FalhaProvedor("claude CLI não encontrado")
        except subprocess.TimeoutExpired:
            raise FalhaProvedor("claude CLI estourou o timeout")
        if processo.returncode != 0:
            raise FalhaProvedor(
                f"claude CLI retornou {processo.returncode}: {processo.stderr.strip()[:200]}"
            )
        texto = (processo.stdout or "").strip()
        if not texto:
            raise FalhaProvedor("claude CLI devolveu resposta vazia")
        return texto


class ProvedorOpenAICompat(Provedor):
    """Provedores com API compatível com OpenAI (ex.: Groq). Ativado no passo 2."""

    def disponivel(self):
        return super().disponivel() and bool(self._chave())

    def _chave(self):
        return self.configuracao.get("chave") or os.environ.get(
            self.configuracao.get("chave_env", ""), ""
        )

    def responder(self, mensagem, contexto):
        chave = self._chave()
        if not chave:
            raise FalhaProvedor("sem chave configurada")
        url = self.configuracao["url_base"].rstrip("/") + "/chat/completions"
        carga = {
            "model": self.configuracao["modelo"],
            "messages": _montar_mensagens(mensagem, contexto),
        }
        corpo = json.dumps(carga).encode()
        requisicao = urllib.request.Request(
            url,
            data=corpo,
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {chave}",
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(requisicao, timeout=self.timeout) as resposta:
                dados = json.loads(resposta.read())
        except urllib.error.HTTPError as erro:
            raise FalhaProvedor(f"http {erro.code}")
        except (urllib.error.URLError, TimeoutError, OSError) as erro:
            raise FalhaProvedor(f"indisponível: {erro}")
        try:
            return dados["choices"][0]["message"]["content"].strip()
        except (KeyError, IndexError):
            raise FalhaProvedor("resposta em formato inesperado")


class ProvedorGeminiREST(Provedor):
    """Google Gemini via REST (generativelanguage). Ativado no passo 2."""

    def disponivel(self):
        return super().disponivel() and bool(self.configuracao.get("chave"))

    def responder(self, mensagem, contexto):
        chave = self.configuracao.get("chave")
        if not chave:
            raise FalhaProvedor("sem chave configurada")
        modelo = self.configuracao["modelo"]
        url = (
            f"https://generativelanguage.googleapis.com/v1beta/models/"
            f"{modelo}:generateContent?key={chave}"
        )
        carga = {"contents": [{"parts": [{"text": _montar_prompt(mensagem, contexto)}]}]}
        corpo = json.dumps(carga).encode()
        requisicao = urllib.request.Request(
            url, data=corpo, headers={"Content-Type": "application/json"}, method="POST"
        )
        try:
            with urllib.request.urlopen(requisicao, timeout=self.timeout) as resposta:
                dados = json.loads(resposta.read())
        except urllib.error.HTTPError as erro:
            raise FalhaProvedor(f"http {erro.code}")
        except (urllib.error.URLError, TimeoutError, OSError) as erro:
            raise FalhaProvedor(f"indisponível: {erro}")
        try:
            return dados["candidates"][0]["content"]["parts"][0]["text"].strip()
        except (KeyError, IndexError):
            raise FalhaProvedor("resposta em formato inesperado")


# Mapa de tipo de provedor -> classe que o implementa.
CLASSES_POR_TIPO = {
    "ollama": ProvedorOllama,
    "claude_cli": ProvedorClaudeCLI,
    "openai_compat": ProvedorOpenAICompat,
    "gemini_rest": ProvedorGeminiREST,
}


# --------------------------------------------------------------------------- #
# Montagem de prompt/mensagens a partir de (mensagem, contexto).
# contexto pode trazer 'sistema' (instrução de sistema) e 'historico' (lista de turnos).
# --------------------------------------------------------------------------- #
def _montar_prompt(mensagem, contexto):
    contexto = contexto or {}
    partes = []
    sistema = contexto.get("sistema")
    if sistema:
        partes.append(sistema)
    for turno in contexto.get("historico", []):
        autor = turno.get("autor", "usuario")
        partes.append(f"{autor}: {turno.get('texto', '')}")
    partes.append(f"usuario: {mensagem}")
    return "\n\n".join(partes)


def _montar_mensagens(mensagem, contexto):
    contexto = contexto or {}
    mensagens = []
    sistema = contexto.get("sistema")
    if sistema:
        mensagens.append({"role": "system", "content": sistema})
    for turno in contexto.get("historico", []):
        papel = "assistant" if turno.get("autor") == "assistente" else "user"
        mensagens.append({"role": papel, "content": turno.get("texto", "")})
    mensagens.append({"role": "user", "content": mensagem})
    return mensagens


# --------------------------------------------------------------------------- #
# Roteamento com cadeia de fallback.
# --------------------------------------------------------------------------- #
def construir_provedores(config):
    """Instancia os provedores na ordem de fallback, ignorando tipos desconhecidos."""
    provedores = []
    declarados = config.get("provedores", {})
    for nome in config.get("ordem_fallback", []):
        configuracao = declarados.get(nome)
        if not configuracao:
            logging.warning("provedor '%s' na ordem mas sem configuração — pulando", nome)
            continue
        classe = CLASSES_POR_TIPO.get(configuracao.get("tipo"))
        if not classe:
            logging.warning("tipo desconhecido para '%s' — pulando", nome)
            continue
        provedores.append(classe(nome, configuracao))
    return provedores


def rotear(mensagem, contexto=None, config=None):
    """Tenta cada provedor da cadeia. Devolve (texto, nome_do_provedor).

    Levanta RuntimeError só se TODOS falharem (não deveria acontecer com o Ollama no fim).
    """
    config = config or carregar_config()
    provedores = construir_provedores(config)
    if not provedores:
        raise RuntimeError("nenhum provedor configurado na ordem_fallback")

    erros = []
    for provedor in provedores:
        if not provedor.disponivel():
            logging.info("[roteador] %s indisponível (pré-checagem) — pulando", provedor.nome)
            erros.append(f"{provedor.nome}: indisponível")
            continue
        try:
            texto = provedor.responder(mensagem, contexto)
            logging.info("[roteador] respondido por '%s'", provedor.nome)
            return texto, provedor.nome
        except FalhaProvedor as falha:
            logging.warning("[roteador] %s falhou: %s — caindo pro próximo", provedor.nome, falha)
            erros.append(f"{provedor.nome}: {falha}")
        except Exception as erro:  # defensivo: nenhum provedor derruba o roteador
            logging.error("[roteador] %s erro inesperado: %s — caindo pro próximo", provedor.nome, erro)
            erros.append(f"{provedor.nome}: erro inesperado {erro}")

    raise RuntimeError("todos os provedores falharam: " + "; ".join(erros))


if __name__ == "__main__":
    import sys

    mensagem = " ".join(sys.argv[1:]) or "Diga apenas: roteador ok."
    texto, provedor = rotear(mensagem)
    print(f"[provedor: {provedor}]\n{texto}")
