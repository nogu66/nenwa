// 型の付いていないパッケージのうち、nenwa が使う分だけの宣言。

declare module "hyperdht" {
  export interface KeyPair {
    publicKey: Buffer;
    secretKey: Buffer;
  }
  export default class DHT {
    static keyPair(seed?: Buffer): KeyPair;
  }
}

declare module "hyperswarm" {
  import type { Duplex } from "node:stream";
  import type { KeyPair } from "hyperdht";

  // 相手とのつながり（Noise で暗号化した Duplex）
  export interface SwarmConnection extends Duplex {
    publicKey: Buffer;
    remotePublicKey: Buffer;
  }
  export interface JoinOptions {
    server?: boolean;
    client?: boolean;
  }
  export interface PeerDiscovery {
    refresh(opts?: JoinOptions): Promise<void>;
    destroy(): Promise<void>;
  }
  export interface SwarmOptions {
    keyPair?: KeyPair;
    bootstrap?: unknown[];
  }
  export default class Hyperswarm {
    constructor(opts?: SwarmOptions);
    on(event: "connection", listener: (conn: SwarmConnection, info: unknown) => void): this;
    join(topic: Buffer, opts?: JoinOptions): PeerDiscovery;
    destroy(): Promise<void>;
  }
}

// テストだけが使う手元用の DHT
declare module "hyperdht/testnet.js" {
  export interface Testnet {
    bootstrap: unknown[];
    destroy(): Promise<void>;
  }
  export default function createTestnet(size?: number): Promise<Testnet>;
}
