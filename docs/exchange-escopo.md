# Exchange em Rust — Documento de Escopo

> Projeto pessoal de aprendizado. **Não é** uma exchange real, não opera com dinheiro
> real, não tem clientes. É um simulador de motor de negociação construído para
> aprender engenharia de sistemas de alta corretude. Este documento define o escopo,
> as regras, a stack e as decisões antes de escrever código.

---

## 1. Visão geral e objetivo

O sistema recebe ordens de compra e venda de um ativo, casa essas ordens segundo
prioridade preço-tempo, liquida os negócios movendo saldos entre contas por partidas
dobradas, e publica o que acontece para assinantes de market data — tudo de forma
durável e recuperável após uma queda.

O objetivo declarado não é o produto, é a demonstração de quatro competências que o
programador mediano não domina:

1. **Determinismo como disciplina de projeto** — mesma sequência de entrada produz
   sempre exatamente o mesmo estado e os mesmos negócios.
2. **A cadeia atômica reservar → casar → liquidar que nunca desbalanceia o dinheiro**,
   sob execução parcial, cancelamento e crash. Este é *o* problema de corretude difícil.
3. **Recuperação via event sourcing / WAL** — reconstruir o estado inteiro relendo o log.
4. **Correção de sessão de protocolo (FIX)** — mensageria confiável com números de
   sequência, recuperação de sessão sem perder nem duplicar ordem.

Tudo que não estiver a serviço desses quatro eixos deve ser mantido pequeno.

---

## 2. Arquitetura — a decisão central

O sistema se divide em duas metades com naturezas opostas:

**Núcleo determinístico (síncrono, single-threaded)**
- Order book, motor de matching, ledger de liquidação, aplicação de eventos.
- Sem `async`, sem paralelismo, sem locks, sem floats, sem leitura de relógio ou de
  fontes não-determinísticas dentro da lógica. O tempo, quando necessário, entra como
  *dado* carregado no comando/evento, nunca lido de dentro.
- Roda numa única thread dedicada, num loop: consome um comando da fila, aplica,
  emite eventos.
- Internamente, matching (por instrumento) e ledger são **componentes isolados que
  conversam por comando/evento**, não por acesso direto ao estado um do outro. Rodam na
  mesma thread agora; a fronteira de mensagem é o que torna thread-por-instrumento uma
  troca mecânica depois (ver decisão 10).

**Borda (assíncrona, concorrente)**
- Gateway de rede, sessões de cliente, checagem de risco com I/O, fanout de market data.
- Aqui vive o `tokio`.

As duas metades se comunicam **por canais**, nunca por memória compartilhada. A borda
serializa os comandos para o núcleo; o núcleo devolve eventos para a borda distribuir.
Padrão de referência: **single-writer / LMAX Disruptor**.

```
   [ tokio: gateway + sessões ]        BORDA (concorrente)
              |  canal (comandos)
              v
   [ THREAD ÚNICA: núcleo ]            NÚCLEO (determinístico)
     risco → WAL → matching → liquidação
              |  canal (eventos)
              v
   [ tokio: market data + respostas ]  BORDA (concorrente)
```

Essa separação é a espinha dorsal do projeto. Se o núcleo virar `async` com locks, o
determinismo, o replay e a recuperação morrem juntos.

---

## 3. Modelo de domínio e regras

### 3.1 Representação de dinheiro e precisão — regra inegociável

**Nunca usar `f64` para dinheiro, preço ou quantidade.** No núcleo, tudo é **inteiro**.
Para eliminar de vez arredondamento no dinheiro, o modelo é:

- **Quantidade em número inteiro de lots** (`i64`). Um *lot* é a menor quantidade
  negociável do ativo base; o tamanho do lot é definido por instrumento.
- **Preço em unidades-mínimas-da-quote por lot** (`i64`). A quote (BRL) é contada em
  centavos.
- **Valor de um negócio = `preço × lots`** — multiplicação inteira pura, **sem divisão,
  sem arredondamento, sem perda**.
- **Tick** = incremento mínimo de preço (ex.: 1 centavo por lot). Preços válidos são
  múltiplos do tick.

Conversão para exibição legível (casas decimais) acontece só na borda. `rust_decimal` é
aceitável na borda; no núcleo, inteiro puro.

### 3.2 Entidades

- **Instrument** — par `base/quote` (ex.: `TESTE/BRL`), com tamanho de lot e tick.
- **Account** — id, e saldos por ativo/moeda divididos em *disponível* e *reservado*.
- **Order** — id, conta, instrumento, lado (bid/ask), tipo (limit/market/stop), preço
  (se limit), quantidade total em lots, quantidade restante, time-in-force, número de
  sequência de chegada, timestamp de parede (dado, só auditoria), estado.
- **Order book** — dois lados; cada lado ordenado por preço, com fila FIFO por nível.
- **Trade / Fill** — resultado de um casamento: id da ordem taker, id da ordem maker,
  preço (o do maker), quantidade em lots.

### 3.3 Regras de matching

- **Prioridade preço-tempo**: melhor preço primeiro; empate no preço resolve por
  **número de sequência de chegada** (FIFO), nunca por relógio de parede.
- O negócio executa **no preço do maker** (quem já estava no livro), nunca no do taker.
- Ordem limite que não casa tudo: o restante **descansa** no livro (se TIF permitir).
- Ordem a mercado: casa contra os melhores níveis até esgotar quantidade ou livro;
  nunca descansa.
- **Time-in-force**: `GTC` (resto descansa), `IOC` (executa o que der, cancela o resto),
  `FOK` (tudo imediatamente ou nada).
- **Stop**: dormente até o preço cruzar o gatilho; então vira market/limit. *Fase tardia.*

### 3.4 Ciclo de vida da ordem (máquina de estados)

Transições ilegais devem ser impossíveis (enum + validação central):

```
Nova ──(risco)──> Rejeitada [terminal]
     └──> Aceita ──> Parcial ──> Preenchida [terminal]
                  ├──> Cancelada [terminal]   (usuário, ou sobra de IOC)
                  └──> Expirada  [terminal]   (FOK sem casar tudo)
```

- **Rejeitada**: falhou validação/risco; nunca tocou o livro.
- **Cancelada**: ação do usuário, ou sobra de uma IOC.
- **Expirada**: FOK que não pôde casar a quantidade inteira imediatamente.
- Estados terminais não retornam.

### 3.5 Invariantes (o que o sistema nunca pode violar)

Checáveis por teste e por asserção; são a definição de corretude:

- **Conservação de valor**: a soma de todos os saldos (disponível + reservado) de cada
  ativo/moeda — **incluindo a conta da casa das taxas** — é constante ao longo de toda a
  vida do sistema. Nenhum negócio cria ou destrói valor.
- **Reserva consistente**: para toda ordem em repouso existe reserva correspondente
  travada. Cancelar libera exatamente o que foi travado.
- **Não-negatividade**: nenhum saldo disponível fica negativo.
- **Determinismo de replay**: aplicar o log do zero produz estado logicamente idêntico.
- **Prioridade respeitada**: nenhum maker é furado por outro de preço pior ou sequência
  posterior.

---

## 4. Regras de negócio por subsistema

### 4.1 Risco pré-trade (porteiro, na borda antes do núcleo)

Antes de a ordem tocar o livro:
- Conta tem disponível suficiente? (compra: quote; venda: base)
- Passa nos limites: tamanho máximo de ordem/posição, sanidade de preço (rejeitar
  *fat-finger* muito fora do mercado), taxa de mensagens por conta.
- Se aprovada, **move o valor de disponível para reservado de forma atômica**. Sem a
  reserva, o mesmo saldo respalda duas ordens e o sistema inventa dinheiro.

Ciclo da reserva: aceitar = reservar; cancelar = devolver; executar = reserva vira
pagamento na liquidação.

### 4.2 Liquidação (settlement) — o ledger de partidas dobradas

Quando um fill acontece, gera lançamentos que se equilibram, **incluindo a perna da
taxa**. Ex.: comprador B leva 2 lots @ 100 de S, com taxa de taker t indo para a conta
da casa H:

```
B:  quote -(200 + t),  base +2
S:  quote +200,        base -2
H:  quote +t
soma de cada coluna = 0
```

Este é o motor de transações por partidas dobradas — módulo interno. Alimentado pelos
fills do matching. Toda liquidação passa pelo WAL antes de valer.

**Taxas**: taker fee em basis points para a conta da casa; maker sem taxa por ora;
configurável (default pode ser não-zero pequeno para exercitar o caminho). A conservação
de valor deve fechar contando a conta da casa.

### 4.3 Feed de market data (lateral ao núcleo)

- **Snapshot** (estado completo do livro) + **incrementos** (deltas).
- Todo update carrega **número de sequência**; o assinante detecta buraco (recebeu 50 e
  52 → pede snapshot novo).
- Nível 1 (topo: melhor bid/ask) e Nível 2 (profundidade completa).
- Só consome eventos do núcleo; nunca dirige o núcleo.

### 4.4 Gateway de protocolo (porta de entrada)

- Traduz mensagem de rede → comando interno (entrada) e evento → mensagem de resposta
  (saída).
- É a **única** superfície que toca entrada não-confiável. Sanitiza tudo antes de passar
  pro núcleo. Atribui aqui o número de sequência de chegada e o timestamp de parede.
- **Fase 1: protocolo próprio em JSON** (`serde_json`) — legível, testável com `netcat`,
  separa erro de parser de erro de lógica. Fase tardia: FIX.

### 4.5 FIX (Financial Information eXchange) — fase tardia, o carimbo de diferenciação

- **Camada de sessão**: Logon, heartbeats, e números de sequência em toda mensagem;
  recuperação de sessão após queda (resend request / gap fill) sem perder nem duplicar.
  Máquina de estados de sessão — o problema difícil de verdade.
- **Camada de aplicação**: NewOrderSingle (`35=D`), ExecutionReport, OrderCancelRequest.
  Formato `tag=valor` delimitado por SOH (`0x01`).
- Estratégia: implementar a lógica de sessão você mesmo é o ponto; não terceirizar numa
  lib que esconde a mecânica.

### 4.6 Log de eventos / WAL (costura tudo)

- Todo comando que muda estado é **anexado ao log durável e sincronizado (`fsync`)
  ANTES** de aplicar no estado em memória, e antes de responder ao cliente.
- Estado (livros + ledger) é *derivado* do log.
- **Layout do registro**: `[tamanho: u32][crc32: u32][payload]`, payload serializado com
  `postcard`. Append-only.
- **Recuperação no boot**: relê o log em ordem; para cada registro confere o crc; se não
  bater ou faltar byte, é *torn write* (rabo rasgado por crash) → descarta e para.
  Reaplica no núcleo determinístico → estado idêntico.
- **Política de `fsync`**: **um fsync por registro** no início (simples e correto).
  *Group commit* (vários registros por fsync) é otimização conquistada com benchmark,
  não assumida.
- Entregas de currículo: trilha de auditoria completa, replay determinístico para debug,
  teste por simulação.

### 4.7 Modelo de tempo

- O núcleo **nunca** lê o relógio.
- Timestamp de parede é atribuído na **borda**, na chegada, e viaja como *dado* no
  comando/evento (uso: auditoria e exibição).
- Prioridade de tempo do matching usa **número de sequência monotônico** atribuído na
  ingestão (= ordem de escrita no log), não o relógio.

---

## 5. Stack técnico

| Camada | Escolha | Porquê |
|---|---|---|
| Linguagem | Rust estável | Corretude via tipos, sem GC, controle de memória |
| Núcleo | Rust síncrono puro | Determinismo; sem `async`, sem locks, sem float |
| Dinheiro (núcleo) | inteiro (`i64`): lots + preço-por-lot | Determinismo e exatidão, sem divisão |
| Dinheiro (borda/exibição) | `rust_decimal` | Casas decimais legíveis |
| Estruturas do livro | `BTreeMap<Price, VecDeque<Order>>` por lado | Preço ordenado (melhor no extremo) + FIFO por nível |
| Índice de cancelamento | `HashMap<OrderId, localização>` | Achar ordem por id em O(1) |
| Serialização | `serde` + `postcard` (WAL/binário), `serde_json` (protocolo fase 1) | Compacto e determinístico / legível |
| WAL | `std::fs`, framing manual, `crc32fast`, `sync_data` | Controle explícito de durabilidade |
| Borda de rede | `tokio` (TCP, task por conexão) | Padrão para I/O assíncrono em Rust |
| Ponte borda↔núcleo | canais (`tokio::mpsc` entrada; `mpsc`/broadcast saída) | Single-writer sem memória compartilhada |
| Fanout de market data | `tokio::broadcast` | Um produtor, muitos assinantes |
| Testes de propriedade | `proptest` | Milhares de sequências afirmando invariantes |
| Benchmark | `criterion` | Throughput/latência com rigor estatístico |
| Observabilidade | `tracing` | Logs estruturados na borda |

Nota sobre `unsafe`: objetivo é **zero `unsafe`** no núcleo. Se aparecer, é sinal de que
algo está sendo forçado.

---

## 6. Segurança e modelo de ameaças

O projeto é um simulador, mas *projetar defesa* é parte da demonstração. Fronteira de
confiança: **o gateway é a única coisa que toca entrada não-confiável; o núcleo nunca vê
bytes crus.**

- **Entrada malformada / maliciosa** → validação total no gateway; inválido é rejeitado
  com erro, nunca chega ao núcleo.
- **Panic como DoS** → o núcleo não pode ter `panic!` alcançável por entrada. Validar
  antes de aplicar; usar `Result`. Panic no núcleo derruba tudo.
- **Criação de valor (o ataque financeiro)** → a invariante de conservação é uma
  propriedade de *segurança*. Qualquer transição que a violaria é rejeitada.
- **Replay / duplicação de mensagem** → números de sequência por sessão (o que o FIX
  resolve); rejeitar sequência repetida ou fora de ordem.
- **Adulteração do log de auditoria** → checksum por entrada; opcionalmente encadeamento
  de hash (cada entrada inclui o hash da anterior) para tornar adulteração detectável.
- **Exaustão de recursos** → limite de tamanho de mensagem, de conexões, throttle por
  sessão, timeout de sessão ociosa.
- **Falha parcial** → o núcleo falha em modo *fail-stop*: se detectar estado
  inconsistente, para em vez de continuar errado.
- **Segredos** → nenhuma credencial no repositório; se houver TLS, tratamento correto de
  certificados.

---

## 7. Regulação e compliance

**Aviso: não sou advogado, e o ponto principal desta seção é que a regulação não se
aplica a você.** Um projeto pessoal, com dinheiro fake, sem clientes e sem operação real
de mercado **não é entidade regulada e não precisa de autorização nenhuma**. Você não
precisa de licença, não registra nada, não infringe nada ao publicar no GitHub.

O valor está em **simular os controles** que a regulação exige — implementá-los mostra
maturidade de infra financeira. Mapa do que existe no Brasil e como cada item vira
engenharia:

- **Marco Legal dos Criptoativos (Lei 14.478/2022)** — prestadores de serviço de ativos
  virtuais (VASPs) só operam mediante autorização do Banco Central, como PJ no Brasil.
  Regulamentado pelas Resoluções BCB 519, 520 e 521 (editadas em nov/2025, vigência a
  partir de fev/2026): autorização, governança, padrões prudenciais.
  → *Engenharia a simular*: **segregação patrimonial** — saldos de cliente jamais
  misturados com o caixa da casa; contas separadas no ledger; invariante que impede
  comingling.
- **PLD/FT — prevenção à lavagem (Lei 9.613/1998, COAF)** → *simular*: campos de
  identidade por conta, ganchos de detecção de atividade suspeita, trilha de auditoria
  completa (o event sourcing já dá de graça).
- **CVM (valores mobiliários, Parecer de Orientação 40)** — se o ativo for valor
  mobiliário, cai noutro regime, sob a CVM. → *decisão de escopo*: manter o ativo
  genérico, sem característica de security, para não entrar nesse regime.
- **LGPD (dados pessoais)** → *simular*: minimização de dados; como você não guarda dado
  pessoal real, o ponto é demonstrar consciência (não guardar o que não precisa).

Resumo: **implemente os controles como exercício de design; não busque conformidade
legal, que não é exigida de um projeto de estudo.**

---

## 8. Boa conduta

**Conduta de engenharia**
- Disciplina de teste: toda invariante coberta por teste de propriedade.
- Reprodutibilidade: qualquer bug reproduzível por replay do log.
- Código auto-documentado por nomes e tipos; comentário só para comportamento não-óbvio.
- Histórico de commits que conta a evolução (fases legíveis).
- Licença explícita (MIT ou Apache-2.0).
- **Honestidade no README**: declarar que é simulador de aprendizado, sem dinheiro real,
  não operante. Não apresentar brinquedo como produção.

**Conduta de mercado (conceitos a projetar, mostram consciência de domínio)**
- Justiça preço-tempo estrita: nenhuma reordenação de ordens.
- Sem front-running: o operador não pode espiar ordens que chegam e se antecipar.
- Ordenação determinística de chegada (o log define a ordem canônica).
- Opcional: prevenção de self-trade / wash trading.
- Trilha de auditoria para responsabilização.

---

## 9. Testes e garantia de corretude

- **Unitários** por operador de matching e por transição do ledger.
- **Propriedade (`proptest`)**: milhares de sequências aleatórias de ordens válidas,
  afirmando ao fim todas as invariantes da seção 3.5 — sobretudo conservação de valor.
- **Recuperação**: simular crash em cada ponto possível da escrita do WAL e afirmar que
  a recuperação restaura estado idêntico (semente do Deterministic Simulation Testing).
- **Replay**: rodar uma sessão, salvar o log, reconstruir do zero, comparar estado.
- **Benchmark (`criterion`)**: throughput de ordens/s e latência p50/p99 do matching.

---

## 10. Escopo: dentro e fora

**Dentro (ao longo das fases)**
- Múltiplos instrumentos de primeira classe (F0 roda um; uma thread de núcleo, N books,
  1 ledger global, com fronteiras de mensagem para thread-por-instrumento futuro).
- Ordens limit e market → stop.
- Time-in-force GTC/IOC/FOK.
- Risco pré-trade com reservas.
- Liquidação por partidas dobradas sobre WAL, com taxa maker/taker.
- Event sourcing + recuperação; snapshots (fase tardia).
- Feed de market data (snapshot + incremental + sequência).
- Gateway com protocolo próprio JSON → FIX.
- Testes de propriedade + simulação + benchmark.

**Fora (explicitamente, para proteger o escopo)**
- Integração com dinheiro/pagamento real, KYC real.
- UI web além do mínimo de demonstração.
- Ativos com característica de valor mobiliário.
- Liquidação on-chain / blockchain.
- Micro-otimização de latência extrema (só depois de correto).
- Consenso distribuído / replicação — *stretch* de longuíssimo prazo, fora do núcleo.
- Derivativos exóticos.

---

## 11. Decisões fechadas

1. **Ativo/par**: instrumento = par `base/quote` (ex.: `TESTE/BRL`), genérico (sem cara
   de security). **Multi-instrumento é cidadão de primeira classe desde o dia zero**: o
   livro vive num `HashMap<InstrumentId, OrderBook>`, toda ordem carrega seu instrumento,
   o ledger permanece global. A F0 roda com um instrumento só para o loop girar, mas a
   forma já é multi — suportar vários vira popular o mapa, não refatorar.
2. **Precisão**: quantidade em lots inteiros; preço em unidades-mínimas-da-quote por
   lot; valor do negócio = `preço × lots` (inteiro puro, sem divisão). Quote em
   centavos. Tick = incremento mínimo de preço por instrumento.
3. **Saldo/ledger**: dentro do núcleo determinístico, como módulo.
4. **Protocolo fase 1**: JSON (`serde_json`). Migra para FIX na fase tardia.
5. **WAL**: registro `[tamanho][crc32][payload postcard]`, append-only; fsync por
   registro no início, group commit depois se o benchmark pedir. Torn write detectado
   por crc no replay.
6. **Snapshot**: adiado até a fase 7; relê log inteiro até lá. Formato: estado completo
   + offset/sequência correspondente.
7. **Taxas**: taker fee em bps para conta da casa; maker zero; configurável; terceira
   perna na liquidação; conservação inclui a conta da casa.
8. **Tempo**: núcleo nunca lê relógio. Timestamp de parede atribuído na borda como dado
   (auditoria). Prioridade FIFO usa número de sequência monotônico da ingestão.
9. **Ciclo de vida da ordem**: máquina de estados da seção 3.4; transições ilegais
   impossíveis.
10. **Multi-instrumento (threading)**: uma thread de núcleo, N order books, 1 ledger
    global, log de sequência global. Para deixar thread-por-instrumento fácil depois sem
    pré-construir threading, desenhar as fronteiras como fronteiras de mensagem já agora:
    (a) matching (por instrumento) e ledger são componentes isolados que conversam por
    comando/evento, nunca por acesso direto ao estado — virar thread depois = trocar
    chamada por `send` em canal; (b) ledger é dono único dos saldos, acessado só por
    mensagens (`reserve`/`release`/`settle`), matching nunca toca saldo direto (padrão
    ator, interface idêntica em single e multi-thread); (c) todo registro do log carrega
    tag de partição (id do instrumento, ou `ledger`) para demultiplexar o log global em
    logs por partição no futuro. **Contraponto**: mesmo assim não há speedup linear — o
    ledger global é o ponto de serialização (todo trade toca 2 contas + casa, em qualquer
    par), então paraleliza-se o matching mas não a liquidação. Particionar o ledger
    preservando atomicidade cruzando partições é problema de sistemas distribuídos (o que
    o VSR do TigerBeetle resolve) — stretch de longuíssimo prazo, não fase próxima.
11. **Observação**: somente leitura, nunca muta o núcleo. Fase 0: CLI/print. Depois:
    comando de query determinístico e/ou endpoint HTTP de status alimentado pelo stream
    de market data. `tracing` para logs.
12. **Métricas / definição de pronto**: ver seção 12.

---

## 12. Fases e definição de "pronto"

Cada fase só é concluída quando o critério é atingido — evita refazer para sempre.

- **F0 — modelo de domínio + matching básico**: arquitetura multi-instrumento (livro em
  `HashMap<InstrumentId, OrderBook>`) rodando com um instrumento só; ordens limit,
  prioridade preço-tempo, submissão por CLI. Componentes matching e ledger já isolados
  por interface. *Pronto quando*: submete limit por CLI e vê fills corretos; testes
  unitários de preço-tempo passam.
- **F1 — market, parcial, TIF**: ordens market, execução parcial, GTC/IOC/FOK. *Pronto
  quando*: `proptest` das invariantes de matching passa em milhares de casos.
- **F2 — contas, risco, liquidação**: reservas, risco pré-trade, ledger de partidas
  dobradas com taxa. *Pronto quando*: conservação de valor holds sob `proptest` com
  reserva/cancel/liquidação, incluindo conta da casa.
- **F3 — WAL e recuperação**: append + fsync + crc, recuperação no boot. *Pronto
  quando*: sobrevive a crash injetado em qualquer ponto da escrita; replay reconstrói
  estado idêntico.
- **F4 — borda de rede**: tokio, protocolo JSON, ponte de canais. *Pronto quando*:
  cliente conecta por TCP, manda ordem JSON, recebe resposta; borda não bloqueia núcleo.
- **F5 — market data**: snapshot + incremental + sequência. *Pronto quando*: assinante
  recebe snapshot e incrementais e detecta gap por sequência.
- **F6 — FIX**: sessão (logon, heartbeat, resend) + aplicação. *Pronto quando*: sessão
  faz logon, heartbeat e recupera de queda sem perder nem duplicar ordem.
- **F7 — endurecimento**: benchmark, snapshots, stop orders, múltiplos instrumentos.
  *Pronto quando*: benchmark reporta throughput/latência (referência: matching > 100k
  ordens/s single-thread, p99 medido); snapshots reduzem tempo de boot.
