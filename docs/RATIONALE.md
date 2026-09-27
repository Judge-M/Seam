# Why split Seam?

The prototype put a bounded worker, a generative capability translator, provider selection, model transport, orchestration, and a client API in one Rust workspace. That made it hard to use a worker without the broker and hard to use routing from a different agent runtime. It also overlapped with existing unified gateways.

The split gives each component one job. SDK runs a small task safely within local limits and expresses reasoning and external needs through replaceable ports. Engine decides which authorized semantic capability fits an intent. The user's gateway supplies concrete model and tool execution. This avoids coupling worker lifecycle to routing and avoids duplicating provider infrastructure.

Authorization precedes learned classification because a low-confidence or incorrect semantic choice must never widen privilege. The decision component chooses from an already filtered set; deterministic code validates its choice, confidence, and arguments. The internal System 1 route is a gateway control operation, not a caller capability, so it cannot become an accidental privilege path or recursive route.

The two products remain useful alone. A direct SDK integration needs no Engine. A different agent runtime can use Engine without SDK types. The optional adapter is the only place that knows both contracts.
