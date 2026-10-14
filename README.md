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

Em desenvolvimento, por fases (ver seção 12 do escopo).

- **F0** — modelo de domínio e matching por prioridade preço-tempo: concluída.
- **F1** — ordens a mercado, execução parcial, GTC/IOC/FOK, cancelamento: concluída.
- **F2** — contas, reservas, ledger de partidas dobradas com taxa de taker, risco pré-trade (tamanho máximo e colar de preço): concluída.
- **F3** — WAL com `fsync` por registro, recuperação no boot e replay determinístico: concluída.
- **F4** — servidor TCP com protocolo JSON por linha; o núcleo roda numa thread própria e a borda `tokio` fala com ele só por canais: concluída.
- **Próxima: F5** — market data: snapshot, incrementos e detecção de buraco por sequência.
- **Depois das fases F4–F7: F8** — cliente de terminal (TUI) que conecta ao servidor e mostra o livro ao vivo. Não há interface web: tudo roda no terminal.

As invariantes da seção 3.5 do escopo são verificadas por `proptest` (milhares de fluxos aleatórios): livro nunca cruzado, prioridade preço-tempo contra um modelo de referência, conservação de valor incluindo a conta da casa, reserva consistente e saldos contra um oráculo independente.

A recuperação é testada injetando crash em cada byte do log, um bit trocado em cada byte e um rabo de zeros: em todos os casos o boot sobe e reconstrói um estado idêntico ao dos comandos que chegaram inteiros ao disco.

## Uso

```
cargo run -p edge
```

Um comando por linha:

| Comando | Efeito |
|---|---|
| `deposit <ativo> <quantia>` | credita a conta (ativos: `TESTE`, `BRL`) |
| `buy\|sell <instrumento> <preço> <lots> [gtc\|ioc\|fok]` | ordem limite |
| `buy\|sell <instrumento> mkt <lots> [max <proteção>] [gtc\|ioc\|fok]` | ordem a mercado; sem `max`, a proteção vem do colar de preço |
| `cancel <instrumento> <id>` | cancela uma ordem descansando |
| `balance` | saldos disponível e reservado |
| `quit` | sai |

Unidades inteiras, sem casas decimais: preço em centavos por lot, quantidade em lots, quantias em unidades-mínimas do ativo. Só existe o instrumento `TESTE/BRL` (tick de 10 centavos, 1 lot = 100 unidades de TESTE), com taxa de taker de 10 bps, no máximo 1.000 lots por ordem e colar de 10%.

```
deposit BRL 10000000
depósito: 10000000 BRL
deposit TESTE 100000
depósito: 100000 TESTE
sell TESTE/BRL 10000 5
#1 descansou: 5 lots em TESTE/BRL
buy TESTE/BRL 10100 3
fill TESTE/BRL: taker #2, maker #1, 3 lots @ 10000, taxa 30
buy TESTE/BRL mkt 4
fill TESTE/BRL: taker #3, maker #1, 2 lots @ 10000, taxa 20
#3 cancelada: 2 lots em TESTE/BRL
balance
TESTE: disponível 100000, reservado 0
BRL: disponível 9999950, reservado 0
```

A conta negocia consigo mesma, então só os 50 centavos de taxa saem dela — foram para a conta da casa.

### Persistência

Com `--wal`, cada comando é gravado no log e sincronizado com o disco (`fsync`) antes de ser aplicado e respondido. Ao subir de novo com o mesmo arquivo, o log é reaplicado e o estado volta igual:

```
$ cargo run -p edge -- --wal exchange.wal
log: 0 comandos reaplicados, 0 bytes descartados
deposit BRL 10000000
depósito: 10000000 BRL
deposit TESTE 100000
depósito: 100000 TESTE
sell TESTE/BRL 10000 5
#1 descansou: 5 lots em TESTE/BRL
quit

$ cargo run -p edge -- --wal exchange.wal
log: 3 comandos reaplicados, 0 bytes descartados
buy TESTE/BRL 10000 2
fill TESTE/BRL: taker #2, maker #1, 2 lots @ 10000, taxa 20
```

Cada registro é `[tamanho u32][crc32 u32][comando em postcard]`, com o crc cobrindo tamanho e comando. Um registro cortado por crash é descartado no boot. Um registro íntegro mas inválido interrompe o boot com erro, em vez de seguir com um estado que não corresponde ao log. Sem `--wal`, tudo fica só em memória.

## Servidor

```
cargo run -p edge --bin server -- [--listen <endereço:porta>] [--wal <caminho>]
```

Escuta em `127.0.0.1:7000` por padrão — só a própria máquina; para expor na rede, passe `--listen` explicitamente. Com `--wal`, persiste e retoma como o CLI. Cada linha enviada é uma mensagem JSON; cada resposta também. Qualquer cliente TCP serve, como `socat` ou `nc`:

```
$ socat - TCP:127.0.0.1:7000
{"type":"login","account":1}
{"type":"logged_in","account":1}
{"type":"deposit","asset":"TESTE","amount":100000}
{"type":"deposited","asset":"TESTE","amount":100000}
{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10000,"lots":5}
{"type":"rested","instrument":"TESTE/BRL","order":1,"lots":5}
```

Em outro terminal, outra conta compra parte dessa oferta:

```
$ socat - TCP:127.0.0.1:7000
{"type":"login","account":2}
{"type":"logged_in","account":2}
{"type":"deposit","asset":"BRL","amount":10000000}
{"type":"deposited","asset":"BRL","amount":10000000}
{"type":"market","instrument":"TESTE/BRL","side":"buy","lots":2}
{"type":"fill","instrument":"TESTE/BRL","taker":2,"maker":1,"lots":2,"price":10000,"fee":20}
{"type":"balance"}
{"type":"balances","balances":[{"asset":"TESTE","available":200,"reserved":0},{"asset":"BRL","available":9979980,"reserved":0}]}
```

Mensagens do cliente (`type` obrigatório; campo desconhecido é erro):

| `type` | Campos | Efeito |
|---|---|---|
| `login` | `account` | primeira mensagem da conexão; a conta da casa (`0`) é recusada |
| `deposit` | `asset`, `amount` | credita a conta da sessão |
| `limit` | `instrument`, `side` (`buy`/`sell`), `price`, `lots`, `tif` opcional (`gtc`/`ioc`/`fok`) | ordem limite |
| `market` | `instrument`, `side`, `lots`, `protection` e `tif` opcionais | ordem a mercado; sem `protection`, vale o colar de preço |
| `cancel` | `instrument`, `order` | cancela uma ordem descansando |
| `balance` | — | saldos da conta da sessão |

O servidor responde com `logged_in`, `deposited`, `deposit_rejected`, `rested`, `fill`, `cancelled`, `expired`, `rejected` (com `code` estável e `message` legível), `balances` e `error`. Erro numa mensagem não derruba a conexão; ela fecha em linha acima de 4 KiB, sessão ociosa por 5 minutos, servidor cheio (1.024 conexões) ou núcleo parado.

## Licença

MIT — ver [`LICENSE`](LICENSE).
