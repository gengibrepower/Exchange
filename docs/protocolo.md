# Servidor: protocolo JSON e feed de market data

[← voltar ao README](../README.pt-BR.md)

```
cargo run -p edge --bin server -- [--listen <endereço:porta>] [--fix <endereço:porta>] [--wal <caminho> [--snapshot-every <n>]]
```

Escuta JSON em `127.0.0.1:7000` e FIX em `127.0.0.1:7001` por padrão — só a própria máquina; para expor na rede, passe `--listen` explicitamente. Com `--wal` e `--snapshot-every`, persiste e retoma como o CLI. Cada linha enviada é uma mensagem JSON; cada resposta também. Qualquer cliente TCP serve, como `socat` ou `nc`:

```
$ socat - TCP:127.0.0.1:7000
{"type":"login","account":1}
{"type":"logged_in","account":1}
{"type":"deposit","asset":"TESTE","amount":100000}
{"type":"deposited","asset":"TESTE","amount":100000}
{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10000,"lots":5}
{"type":"rested","instrument":"TESTE/BRL","order":1,"side":"sell","price":10000,"lots":5}
```

Em outro terminal, outra conta compra parte dessa oferta:

```
$ socat - TCP:127.0.0.1:7000
{"type":"login","account":2}
{"type":"logged_in","account":2}
{"type":"deposit","asset":"BRL","amount":10000000}
{"type":"deposited","asset":"BRL","amount":10000000}
{"type":"market","instrument":"TESTE/BRL","side":"buy","lots":2}
{"type":"fill","instrument":"TESTE/BRL","role":"taker","order":2,"lots":2,"price":10000,"fee":20}
{"type":"balance"}
{"type":"balances","balances":[{"asset":"TESTE","available":200,"reserved":0},{"asset":"BRL","available":9979980,"reserved":0},{"asset":"OUTRO","available":0,"reserved":0}]}
```

E, no primeiro terminal, a conta 1 fica sabendo na hora, sem ter mandado nada:

```
{"type":"fill","instrument":"TESTE/BRL","role":"maker","order":1,"lots":2,"price":10000,"fee":0}
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
| `instruments` | — | a especificação de cada instrumento: `symbol`, `base`, `quote`, `tick`, `lot_size` e `max_lots` (não exige login) |
| `orders` | — | as ordens abertas da conta, inclusive as de antes da conexão |

Toda ordem (`limit`, `market`, `stop`) aceita um `tag` opcional, um número inteiro não negativo escolhido pelo cliente.

O servidor responde com `logged_in`, `accepted`, `deposited`, `deposit_rejected`, `rested`, `fill`, `cancelled`, `expired`, `armed` (stop aceito e dormente), `triggered` (o stop disparou; os eventos dele vêm em seguida), `rejected` (com `code` estável e `message` legível), `balances`, `instruments`, `orders` e `error`. O `fill` fala só da ordem de quem recebe: `role` diz se ela era `taker` ou `maker`, `fee` é o que essa conta pagou (o maker não paga), e um autonegócio vira duas linhas, uma por ordem. O `rested` traz o `side` e o `price` em que a sobra ficou no livro.

Uma ordem com `tag` recebe primeiro um `accepted` com o `tag` e o id que o servidor deu a ela, e só depois os relatórios da própria ordem; um erro de validação (instrumento desconhecido, preço zero) volta como `error` com o mesmo `tag`. É o jeito de o cliente saber qual id é de qual envio. O servidor não guarda o `tag` nem confere se ele é único: só o devolve. `accepted` quer dizer recebida, não aceita no livro, e pode vir seguida de um `rejected`. Sem `tag`, nada muda nas respostas.

```
{"type":"limit","instrument":"TESTE/BRL","side":"sell","price":10100,"lots":1,"tag":7}
{"type":"accepted","tag":7,"order":3}
{"type":"rested","instrument":"TESTE/BRL","order":3,"side":"sell","price":10100,"lots":1}
```

O `orders` lista as ordens descansando no livro (`"state":"resting"`) e os stops dormentes (`"armed"`), em ordem de id, com o pedido original (`type`, `price`, `trigger`, `tif`, `lots`) e o que falta (`remaining`). A resposta mostra o estado no ponto em que a consulta entrou na fila do núcleo e vem no mesmo fluxo dos relatórios: o que já estava a caminho chega antes dela (e está refletido nela), e o que chega depois aconteceu depois. Quem aplica os relatórios e depois a lista não conta nada duas vezes.

Cada sessão recebe tudo que acontece com a sua conta, inclusive o que vem do comando de outra sessão: o fill de uma ordem descansando, o disparo de um stop. Nunca recebe o que é de outra conta: quem dispara o stop alheio vê o próprio negócio e nada do stop. Tudo chega num fluxo só, na ordem do núcleo, então o fill que explica a recusa de um cancelamento chega antes dela. Uma sessão que acumula 1.024 entregas sem ler é desconectada com um `error`: o núcleo não espera ninguém, e descartar em silêncio perderia um fill que não há como pedir de novo.

A quem assina o feed, o servidor envia `snapshot` e depois os incrementos `depth` (quantidade absoluta no preço, `0` quando o nível some), `top` e `trade`. Erro numa mensagem não derruba a conexão; ela fecha em linha acima de 4 KiB, sessão ociosa por 5 minutos, servidor cheio (1.024 conexões) ou núcleo parado.


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
