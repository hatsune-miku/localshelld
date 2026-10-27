type Descriptor = Omit<PublicKeyCredentialDescriptor, 'id'> & { id: string }
export type Creation = Omit<PublicKeyCredentialCreationOptions, 'challenge' | 'user' | 'excludeCredentials'> & {
  challenge: string
  user: Omit<PublicKeyCredentialUserEntity, 'id'> & { id: string }
  excludeCredentials?: Descriptor[]
}
export type Assertion = Omit<PublicKeyCredentialRequestOptions, 'challenge' | 'allowCredentials'> & {
  challenge: string
  allowCredentials?: Descriptor[]
}

function decode(value: string): ArrayBuffer {
  const text = atob(value.replace(/-/g, '+').replace(/_/g, '/'))
  return Uint8Array.from(text, (character) => character.charCodeAt(0)).buffer
}

function encode(value: ArrayBuffer): string {
  return btoa(String.fromCharCode(...new Uint8Array(value)))
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=/g, '')
}

function available() {
  if (!window.isSecureContext || !window.PublicKeyCredential || !navigator.credentials)
    throw new Error('请使用支持通行密钥的浏览器，并通过 HTTPS 或 localhost 打开。')
}

export async function createCredential(options: Creation) {
  available()
  const credential = (await navigator.credentials.create({
    publicKey: {
      ...options,
      challenge: decode(options.challenge),
      user: { ...options.user, id: decode(options.user.id) },
      excludeCredentials: options.excludeCredentials?.map((item) => ({ ...item, id: decode(item.id) })),
    },
  })) as PublicKeyCredential | null
  if (!credential) throw new Error('未创建通行密钥。')
  const response = credential.response as AuthenticatorAttestationResponse
  return {
    id: credential.id,
    rawId: encode(credential.rawId),
    type: credential.type,
    response: {
      attestationObject: encode(response.attestationObject),
      clientDataJSON: encode(response.clientDataJSON),
      transports: response.getTransports?.() ?? [],
    },
    extensions: credential.getClientExtensionResults(),
  }
}

export async function getCredential(options: Assertion) {
  available()
  const credential = (await navigator.credentials.get({
    publicKey: {
      ...options,
      challenge: decode(options.challenge),
      allowCredentials: options.allowCredentials?.map((item) => ({ ...item, id: decode(item.id) })),
    },
  })) as PublicKeyCredential | null
  if (!credential) throw new Error('未完成通行密钥验证。')
  const response = credential.response as AuthenticatorAssertionResponse
  return {
    id: credential.id,
    rawId: encode(credential.rawId),
    type: credential.type,
    response: {
      authenticatorData: encode(response.authenticatorData),
      clientDataJSON: encode(response.clientDataJSON),
      signature: encode(response.signature),
      userHandle: response.userHandle ? encode(response.userHandle) : null,
    },
    extensions: credential.getClientExtensionResults(),
  }
}
