# Cliente de terminal (TUI)

[← voltar ao README](../README.pt-BR.md)

```
cargo run -p tui -- --account <conta> [--server <endereço:porta>]
```

Cliente de terminal que fala com o servidor só pelo [protocolo JSON](protocolo.md), pela crate `exchange-protocol`: o núcleo nem está no grafo de dependências dele. Com o servidor no ar (`127.0.0.1:7000` por padrão), a TUI faz login, pede os `instruments` e assina o L1 e o L2 de cada um.

- **Mercado**: compra, venda, spread e último negócio de cada instrumento, com ▲/▼ em relação ao negócio anterior.
- **Uma aba por instrumento**: o livro L2, com as vendas em cima e as compras embaixo, barras de profundidade e o spread no meio; os negócios, com a hora de chegada; as ordens da conta naquele instrumento, abertas primeiro e as últimas 20 terminadas esmaecidas embaixo; e os saldos.
- **Boleta**, uma por instrumento, que guarda os últimos valores: limite, mercado ou stop (gatilho e, opcional, limite), lots e TIF. Sem modo de inserção: `↵` num nível do livro traz o preço, `h`/`l` anda entre os campos, `j`/`k` sobe e desce (o preço de tick em tick), e os dígitos entram pela direita como centavos (`1 0 0 5 0` é 100,50); o primeiro dígito num campo começa um valor novo. `↵` envia na hora.
- **Cancelamento**: `x` na ordem sob o cursor, confirmado com `y`.
- As ordens abertas antes da TUI subir vêm da consulta `orders` no login; o resto, dos relatórios da conta. Cada ordem da boleta vai com um `tag` próprio, e o `accepted` diz qual id ela recebeu: a linha em Minhas ordens nasce com o tipo, o TIF e o total que foram enviados, mesmo com outra sessão da conta negociando ao mesmo tempo. O saldo é pedido no login e de novo a cada relatório, um pedido por vez.
- Um nível do livro que muda fica com os lots em laranja por cerca de um segundo.
- Um buraco na sequência do feed descarta aquele livro e pede um snapshot novo, sem intervenção.
- **Queda do servidor**: a TUI mostra `● reconectando (n)` e tenta de novo a cada 2s; enquanto isso o livro fica cinza (são os números do instante da queda) e nada é enviado. Ao voltar, refaz o login, os instrumentos, as assinaturas, as ordens e os saldos; a aba aberta e as boletas ficam. Uma ordem que estava aberta e não vem mais na consulta `orders` terminou enquanto a TUI estava fora e aparece como `encerr.`: o servidor não diz como ela terminou.

| Tecla | Efeito |
|---|---|
| `j` `k` / `↓` `↑` | mover; na boleta, ajustar o campo |
| `h` `l` / `←` `→` | na boleta, campo anterior / próximo |
| `tab` / `shift-tab` | foco: livro → ordens → boleta |
| `[` `]` | aba anterior / próxima |
| `↵` | na aba Mercado, abrir o instrumento; no livro, levar o preço à boleta; na boleta, enviar |
| `0`–`9` `⌫` | na boleta, digitar |
| `b` `s` | lado da boleta: compra / venda |
| `x` | nas ordens, cancelar (pede `y`) |
| `?` | ajuda |
| `q` | sair (pede `y` se houver ordem aberta) |
| `ctrl-c` | sair na hora |

## Critério de pronto da F8, à mão

Em três terminais: o servidor (`cargo run -p edge --bin server`), a TUI (`cargo run -p tui -- --account 1`) e outro cliente (`socat - TCP:127.0.0.1:7000`). Pelo `socat`, deposite BRL na conta 1 (`{"type":"login","account":1}` e `{"type":"deposit","asset":"BRL","amount":100000000}`), saia e entre como a conta 2 para negociar (`{"type":"login","account":2}`, `{"type":"deposit","asset":"TESTE","amount":100000}`, `{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10050,"lots":7}`). Na TUI, `]` abre TESTE/BRL e a venda aparece no livro, com os lots em laranja; `k` e `↵` levam o preço à boleta, `l` `3` `↵` compra 3 lots e o fill aparece na linha de status, em Minhas ordens, em Negócios e nos saldos. Uma compra abaixo do preço (`h` `9990` `↵`) fica aberta; em Minhas ordens, `x` e `y` a cancelam.
