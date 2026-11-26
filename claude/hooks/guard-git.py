#!/usr/bin/env python3
"""PreToolUse (Bash): bloqueia commit/push e escrita em branches protegidas.

Protocolo de hook do Claude Code: recebe o evento em JSON no stdin; sair com
código 2 nega a chamada e o stderr volta para o modelo como motivo.
"""
import json, re, shlex, sys

# Subcomandos que gravam histórico ou mexem no remoto: sempre negados.
ALWAYS = {"commit", "push", "merge", "rebase", "cherry-pick", "revert", "am",
          "update-ref", "tag", "notes", "pull", "filter-branch", "replace",
          "clean", "add", "rm", "mv", "config", "apply", "stash", "gc", "prune"}
# Subcomandos que só são negados quando envolvem uma branch protegida.
WITH_PROTECTED = {"checkout", "switch", "reset", "branch", "worktree", "restore"}
PROTECTED = re.compile(
    r"^(?:refs/heads/|origin/|refs/remotes/origin/)?"
    r"(master|main|dev|develop|development|homol|homolog|homologacao|homologation|"
    r"hml|staging|stage|prod|production|producao|release(?:/.*)?|hotfix)$", re.I)
SEPARATORS = {";", "&&", "||", "|", "&", "(", ")", "\n"}
OPTS_WITH_VALUE = {"-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path"}


def segments(cmd):
    lex = shlex.shlex(cmd, posix=True, punctuation_chars=";&|()")
    lex.whitespace_split = True
    try:
        toks = list(lex)
    except ValueError:  # aspas desbalanceadas: decide pelo texto bruto
        return None
    seg = []
    for t in toks:
        if t in SEPARATORS:
            if seg:
                yield seg
            seg = []
        else:
            seg.append(t)
    if seg:
        yield seg


def check_git(args):
    """args: tokens depois de 'git'. Retorna motivo de bloqueio ou None."""
    i = 0
    while i < len(args) and args[i].startswith("-"):
        i += 2 if args[i] in OPTS_WITH_VALUE else 1
    if i >= len(args):
        return None
    sub, rest = args[i], args[i + 1:]
    if sub in ALWAYS:
        return f"git {sub} é proibido durante o review"
    if sub in WITH_PROTECTED and any(PROTECTED.match(a) for a in rest):
        return f"git {sub} envolvendo branch protegida é proibido"
    return None


def reason_for(cmd):
    segs = segments(cmd)
    if segs is None:
        if re.search(r"\bgit\b.*\b(commit|push)\b", cmd):
            return "comando git não analisável contendo commit/push"
        return None
    for seg in segs:
        for j, tok in enumerate(seg):
            if tok == "git" or tok.endswith("/git"):
                r = check_git(seg[j + 1:])
                if r:
                    return r
            # bash -c "git commit ...", sh -c, eval
            if tok in ("-c", "eval") and j + 1 < len(seg) and "git" in seg[j + 1]:
                r = reason_for(seg[j + 1])
                if r:
                    return r
    return None


def main():
    ev = json.load(sys.stdin)
    if ev.get("tool_name") != "Bash":
        return 0
    r = reason_for(ev.get("tool_input", {}).get("command", ""))
    if r:
        print(f"Bloqueado pelo PR Review Orchestrator: {r}.", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
