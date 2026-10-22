# Benchmarks

[← voltar ao README](../README.pt-BR.md)

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

## WAL

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

## Group commit de ponta a ponta

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

## Boot com snapshot

```
cargo bench -p edge --bench boot
```

Monta um log com um fluxo determinístico de 16 contas negociando TESTE/BRL — ordens limite dos dois lados, um quinto delas cruzando o spread, 10% de cancelamentos — e mede o boot de três jeitos: replay completo, snapshot cobrindo o log todo e snapshot com 1% do log depois dele. Mede também as duas metades de gravar um snapshot: montar o estado em bytes e gravar o arquivo com os `fsync`. Cada caso roda 5 vezes, com o log já no cache de páginas, e fica a mediana.

Na mesma máquina e no mesmo NVMe:

| Log | Ordens abertas no fim | Log / snapshot | Replay completo | Snapshot | Snapshot + 1% | Só ler o log |
|---|---|---|---|---|---|---|
| 100 mil comandos | 51 mil | 2,7 MB / 0,8 MB | 67 ms | 28 ms | 27–33 ms | 6 ms |
| 1 milhão de comandos | 510 mil | 28 MB / 8,1 MB | 1,33 s | 370 ms | 390 ms | 83 ms |

Com 1 milhão de comandos, o boot fica 3,5 vezes mais rápido. O que sobra tem duas partes:

- **Ler o log inteiro** (~83 ms): o WAL confere o crc de todo quadro, inclusive dos que o snapshot cobre. Cortar o log depois do snapshot tiraria esse custo, mas mudaria a decisão 6, que guarda o log inteiro; fica para quando o tamanho do log pedir.
- **Carregar o snapshot** (~280 ms, cerca de 0,55 µs por ordem aberta): decodificar o estado, validar cada ordem e reconstruir livro, índices e reservas. Cresce com o livro, não com o log: um log de 10 milhões de comandos com o mesmo livro custaria o mesmo.

Gravar um snapshot desse tamanho custa uns 25 ms para montar o estado e 15 ms para gravar o arquivo, na thread do núcleo: é o pico de latência que aparece a cada `--snapshot-every` comandos.
