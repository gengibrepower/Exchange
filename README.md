# Exchange (simulador) — Rust

Simulador de motor de negociação escrito em Rust, como **projeto pessoal de aprendizado**.

> **Não é uma exchange real.** Não opera com dinheiro real, não tem clientes, não está em produção. Existe para estudar engenharia de sistemas de alta corretude: matching determinístico, liquidação por partidas dobradas, event sourcing / WAL e sessão de protocolo FIX.

## Objetivo

Demonstrar quatro competências:

1. Determinismo como disciplina de projeto — mesma entrada, mesmo estado, sempre.
2. A cadeia atômica reservar → casar → liquidar que nunca desbalanceia o dinheiro.
3. Recuperação via event sourcing / WAL — estado reconstruído relendo o log.
4. Corretude de sessão de protocolo (FIX) — mensageria confiável, sem perder nem duplicar ordem.

## Documentação

- [`docs/escopo.md`](docs/escopo.md) — escopo, arquitetura, regras de domínio e decisões fechadas.
- [`CLAUDE.md`](CLAUDE.md) — instruções para assistência de IA no repositório.

## Status

Em desenvolvimento, por fases (ver seção 12 do escopo). Fase atual: setup / F0 (modelo de domínio + matching básico).

## Licença

MIT — ver [`LICENSE`](LICENSE).IT
