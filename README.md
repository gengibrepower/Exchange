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
- **F5** — feed de market data: snapshot e incrementos numerados por fluxo (L1 e L2), publicados sem que assinante lento segure o núcleo; o cliente `feed` detecta buraco pela sequência e se recupera: concluída.
- **F6** — FIX 4.4 escrito à mão: codec `tag=valor`, sessão (logon, heartbeat, TestRequest, ResendRequest/GapFill, logout) e ordens por `NewOrderSingle`/`OrderCancelRequest` com `ExecutionReport`; uma queda de conexão é recuperada sem perder nem duplicar ordem: concluída.
- **Em andamento: F7** — endurecimento: benchmarks, group commit no WAL, segundo instrumento e stop orders feitos; faltam o aviso ao dono quando a ordem dele é afetada pelo comando de outra sessão, e snapshots.
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
| `deposit <ativo> <quantia>` | credita a conta (ativos: `TESTE`, `OUTRO`, `BRL`) |
| `buy\|sell <instrumento> <preço> <lots> [gtc\|ioc\|fok]` | ordem limite |
| `buy\|sell <instrumento> mkt <lots> [max <proteção>] [gtc\|ioc\|fok]` | ordem a mercado; sem `max`, a proteção vem do colar de preço |
| `buy\|sell <instrumento> <preço\|mkt> <lots> stop <gatilho> [gtc\|ioc\|fok]` | stop: dormente até um negócio chegar ao gatilho (compra: no gatilho ou acima; venda: no gatilho ou abaixo); aí entra como limite em `<preço>` ou a mercado |
| `cancel <instrumento> <id>` | cancela uma ordem descansando ou um stop dormente |
| `balance` | saldos disponível e reservado |
| `quit` | sai |

Unidades inteiras, sem casas decimais: preço em centavos por lot, quantidade em lots, quantias em unidades-mínimas do ativo. Há dois instrumentos, com taxa de taker de 10 bps nos dois:

| Instrumento | Tick | 1 lot | Máximo por ordem | Colar |
|---|---|---|---|---|
| `TESTE/BRL` | 10 centavos | 100 unidades de TESTE | 1.000 lots | 10% |
| `OUTRO/BRL` | 5 centavos | 10 unidades de OUTRO | 500 lots | 5% |

Cada instrumento tem o seu livro e o seu feed, e os dois dividem o mesmo ledger: o BRL travado por uma ordem num livro não pode ser usado no outro.

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

Com `--wal`, cada comando é gravado no log e sincronizado com o disco (`fsync`) antes de ser aplicado e respondido. No servidor, os comandos que chegam juntos dividem um mesmo `fsync` (group commit), sem mudar nada do que é gravado. Ao subir de novo com o mesmo arquivo, o log é reaplicado e o estado volta igual:

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
cargo run -p edge --bin server -- [--listen <endereço:porta>] [--fix <endereço:porta>] [--wal <caminho>]
```

Escuta JSON em `127.0.0.1:7000` e FIX em `127.0.0.1:7001` por padrão — só a própria máquina; para expor na rede, passe `--listen` explicitamente. Com `--wal`, persiste e retoma como o CLI. Cada linha enviada é uma mensagem JSON; cada resposta também. Qualquer cliente TCP serve, como `socat` ou `nc`:

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
| `stop` | `instrument`, `side`, `trigger`, `lots`, `limit` e `tif` opcionais | stop dormente até o último negócio chegar a `trigger`; aí vira limite em `limit` ou, sem ele, a mercado |
| `cancel` | `instrument`, `order` | cancela uma ordem descansando ou um stop dormente |
| `balance` | — | saldos da conta da sessão |
| `subscribe` | `instrument`, `feed` (`l1`/`l2`) | assina o feed (não exige login); assinar de novo traz um snapshot novo |

O servidor responde com `logged_in`, `deposited`, `deposit_rejected`, `rested`, `fill`, `cancelled`, `expired`, `armed` (stop aceito e dormente), `triggered` (o stop disparou; os eventos dele vêm em seguida), `rejected` (com `code` estável e `message` legível), `balances` e `error`. Cada sessão só recebe eventos das ordens que ela criou: quem dispara o stop de outra conta vê o próprio negócio e nada do stop; a quem assina, envia `snapshot` e depois os incrementos `depth` (quantidade absoluta no preço, `0` quando o nível some), `top` e `trade`. Erro numa mensagem não derruba a conexão; ela fecha em linha acima de 4 KiB, sessão ociosa por 5 minutos, servidor cheio (1.024 conexões) ou núcleo parado.

## FIX

A porta FIX fala FIX 4.4, com codec e sessão implementados à mão no crate `exchange-fix` (puro: não lê relógio nem toca rede; o instante entra como dado). O `SenderCompID` é o número da conta (a casa, `0`, é recusada) e o `TargetCompID` é `EXCHANGE`; o cliente é o initiator e propõe o `HeartBtInt` no Logon.

- **Sessão**: todo `MsgSeqNum` é conferido. Buraco gera um único `ResendRequest`; o reenvio volta com `PossDupFlag=Y`, e as mensagens administrativas viram `SequenceReset-GapFill`. Duplicata abaixo do esperado com `PossDupFlag` é ignorada; sem ela, a sessão encerra com Logout. Silêncio além do intervalo gera `TestRequest` e, sem resposta, a conexão cai. Seqs e mensagens enviadas sobrevivem à queda da conexão: ao reconectar, o Logon revela o que faltou.
- **Aplicação**: `NewOrderSingle` (`35=D`, `OrdType` 1 mercado, 2 limite, 3 stop e 4 stop-limite, com o gatilho em `StopPx`, tag 99) e `OrderCancelRequest` (`35=F`) entram; saem `ExecutionReport` (`35=8`: New, Trade, Canceled, Expired, Rejected e Triggered, `ExecType=L`, quando o stop dispara), `OrderCancelReject` (`35=9`) e `BusinessMessageReject` (`35=j`). `ClOrdID` já usado é recusado sem chegar ao núcleo, o que protege o cliente que, na dúvida depois de uma queda, manda a mesma ordem de novo.
- **Desvios do padrão**, os mesmos da borda JSON: preço inteiro por lot (sem casas decimais), sem `AvgPx`, e só quem enviou a ordem que gerou o negócio recebe o relatório: o dono de uma ordem descansando ou de um stop disparado pelo comando de outra sessão não é avisado.

A garantia "nem perde nem duplica" é testada de duas formas: um `proptest` com duas sessões num TCP simulado que cai em pontos aleatórios, perde o que estava em trânsito e recebe reenvios a mais (cada lado recebe exatamente o que o outro enviou, uma vez e em ordem); e um teste de integração com o servidor real, em que a conexão cai depois de a ordem chegar ao núcleo e antes de a confirmação ser lida.

## Feed de market data

Cada instrumento tem dois fluxos, cada um com sua sequência contígua: **L2** (profundidade: cada nível de preço e os negócios) e **L1** (topo do livro e os negócios). Quem assina recebe um `snapshot` com o `seq` e, depois, só os incrementos seguintes. O núcleo nunca espera assinante: quem fica para trás perde mensagens, vê o salto na sequência e assina de novo para receber um snapshot novo.

O cliente `feed` faz isso e desenha o livro no terminal a cada mudança:

```
cargo run -p edge --bin feed -- [--connect <endereço:porta>] [--instrument <símbolo>] [--feed l1|l2] [--plain]
```

Com o servidor rodando e outra conexão negociando:

```
TESTE/BRL  L2  seq 5
        preço     lots
venda   10100        2
venda   10000        3
------ spread 100 ------
compra   9900        3
último negócio: 2 lots @ 10000
```

Num buraco, ele avisa (`buraco na sequência: esperava 6, veio 9 — pedindo snapshot novo`) e segue com o livro reconstruído. `--plain` imprime cada redesenho em sequência em vez de limpar a tela.

## Benchmark

```
cargo bench -p exchange-core
```

Mede `Engine::apply` inteiro (risco pré-trade, reserva, matching e liquidação no ledger), sem WAL nem rede, sobre um fluxo determinístico de ordens limite em torno de um preço médio, em três cenários: `resting` (nada cruza), `crossing` (metade cruza o spread) e `with_cancels` (40% cruza, 20% são cancelamentos). Antes de medir, o fluxo é rodado uma vez e o benchmark falha se mais de 1% dos comandos for rejeitado: rejeitar é o caminho barato e daria um número falso.

- `matching` (`criterion`): throughput, 10 mil comandos por iteração, cada uma num engine novo.
- `latency`: cada `apply` cronometrado em 100 mil comandos, com os percentis pelo método nearest-rank.

Numa AMD Ryzen 5 PRO 5650U, perfil `bench`:

| Cenário | Throughput | p50 | p99 | p99.9 | máx |
|---|---|---|---|---|---|
| `resting` | 2,87 M comandos/s | 321 ns | 742 ns | 5,24 µs | 3,71 ms |
| `crossing` | 1,89 M comandos/s | 311 ns | 2,62 µs | 3,87 µs | 1,15 ms |
| `with_cancels` | 2,14 M comandos/s | 251 ns | 2,26 µs | 3,67 µs | 230 µs |

A referência da seção 12 do escopo é mais de 100 mil ordens/s numa thread. O máximo, na casa dos milissegundos, é uma amostra isolada; o p99.9 mostra que a cauda fica em poucos microssegundos.

### WAL

```
cargo bench -p exchange-wal
```

Mede o custo de um registro durável com payload do tamanho de um comando real (21 bytes). Os arquivos ficam em `target/tmp`, no disco: num `tmpfs` o `fsync` não faz nada e o número seria falso.

- `append`: o `Wal::append` de hoje, um `fsync` por registro.
- `no_fsync`: o mesmo frame sem `fsync`. Só para comparação, nunca uma opção: sem ele o registro não sobrevive a queda de energia.
- `batch/{1,8,64}`: `Wal::append_batch` com N registros, um `write` e um `fsync` para todos.
- `latency`: p50/p99/p99.9/máx de 5 mil `append`.

Num NVMe SK Hynix com btrfs, em várias rodadas (o `fsync` oscila bastante de uma rodada para outra):

| Cenário | Por registro | Registros/s |
|---|---|---|
| `append` | 2–4 ms | 250–450 |
| `no_fsync` | 6–7 µs | ~150 mil |
| `batch/8` | 0,3–0,6 ms | 1,8–3,5 mil |
| `batch/64` | 25–75 µs | 14–41 mil |

Latência do `append`: p50 de 0,7 a 4,4 ms, p99 em torno de 5 ms, máximo de dezenas de ms.

O `fsync` é praticamente todo o custo, e um lote o divide pelo número de registros. Com um `fsync` por comando, o servidor com `--wal` fica em centenas de comandos/s, enquanto o núcleo faz milhões: é o caso que a decisão 5 previa para adotar group commit.

### Group commit de ponta a ponta

```
cargo bench -p edge --bench group_commit
```

Sessões concorrentes falando com a thread do núcleo pelo `CoreHandle`, como as conexões do servidor, cada uma em closed loop: manda um depósito e só manda o próximo depois da resposta. São 2.048 comandos no total, divididos entre as sessões, com o WAL no disco e, para comparação, só em memória.

Com uma sessão, a fila nunca tem mais de um pedido e cada comando paga um `fsync` inteiro. Com mais sessões, os pedidos que chegam durante um `fsync` entram juntos no próximo, e um `fsync` atende vários clientes. Ninguém espera de propósito por um lote.

| Sessões | Memória | WAL |
|---|---|---|
| 1 | 50–64 mil cmd/s | 430–770 cmd/s |
| 8 | 260–280 mil cmd/s | 1,6–3,7 mil cmd/s |
| 64 | 450–660 mil cmd/s | 7–21 mil cmd/s |

Com 64 sessões, o WAL atende de 10 a 30 vezes mais comandos por segundo que com uma, e cada cliente continua esperando mais ou menos um `fsync` por comando. A coluna de memória é o teto sem disco: o custo dos canais e da troca de thread. Por ela, uma sessão sozinha passa uns 15–20 µs só indo e voltando do núcleo.

## Licença

MIT — ver [`LICENSE`](LICENSE).
