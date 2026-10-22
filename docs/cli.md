# CLI, persistência e snapshots

[← voltar ao README](../README.pt-BR.md)

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

## Persistência

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

### Snapshots

Com `--snapshot-every <n>` (só junto com `--wal`), o estado inteiro é gravado em `<wal>.snapshot` a cada `n` comandos e também no boot, se algo foi reaplicado. No boot seguinte, o estado sai do snapshot e só os comandos depois dele são reaplicados:

```
$ cargo run -p edge -- --wal exchange.wal --snapshot-every 2
log: 0 comandos reaplicados, 0 bytes descartados
deposit BRL 100000000
depósito: 100000000 BRL
deposit TESTE 100000
depósito: 100000 TESTE
sell TESTE/BRL 10000 5
#1 descansou: 5 lots em TESTE/BRL
quit

$ cargo run -p edge -- --wal exchange.wal
log: 1 comandos reaplicados, 0 bytes descartados
snapshot: carregado, cobre 2 comandos
buy TESTE/BRL 10000 2
fill TESTE/BRL: taker #2, maker #1, 2 lots @ 10000, taxa 20
```

O snapshot é só um atalho: o log continua inteiro e é ele a verdade. O snapshot guarda o que não dá para reconstruir — ordens abertas na ordem da fila, stops dormentes, último negócio, saldos e os contadores de `seq` e de id de ordem —, e o resto (índices, reservas, oferta de cada ativo, livro do feed) é recalculado na carga. A carga confere as invariantes da seção 3.5 do escopo antes de aceitar o estado.

- A gravação é atômica: vai para um temporário, que é sincronizado e renomeado por cima do anterior, e o diretório é sincronizado em seguida. Um crash em qualquer ponto deixa o snapshot antigo ou o novo inteiro. O arquivo é `[EXSNAP01][tamanho][crc32][payload]`.
- Snapshot ausente, danificado ou recusado pela configuração atual: o boot avisa e reaplica o log inteiro. Snapshot que cobre mais comandos do que o log tem interrompe o boot, porque aí não há como saber qual dos dois é a verdade.
- Falha ao gravar um snapshot só gera um aviso: o log está íntegro, e o atalho volta na próxima tentativa.
- As sequências do feed recomeçam do zero em todo boot: as conexões caíram, e todo assinante começa por um snapshot do feed.
