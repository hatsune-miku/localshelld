import { type LoginResult, api } from './api'

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

type Descriptor = Omit<PublicKeyCredentialDescriptor, 'id'> & { id: string }
type Creation = Omit<PublicKeyCredentialCreationOptions, 'challenge' | 'user' | 'excludeCredentials'> & {
  challenge: string
  user: Omit<PublicKeyCredentialUserEntity, 'id'> & { id: string }
  excludeCredentials?: Descriptor[]
}
type Request = Omit<PublicKeyCredentialRequestOptions, 'challenge' | 'allowCredentials'> & {
  challenge: string
  allowCredentials?: Descriptor[]
}

function available() {
  if (location.hostname !== 'localhost')
    throw new Error('通行密钥需要 localhost 地址。请使用页面中的 localhost 链接重新打开。')
  if (!window.PublicKeyCredential || !navigator.credentials)
    throw new Error('此浏览器不支持通行密钥，请使用支持 WebAuthn 的浏览器。')
}

export async function registerPasskey(name: string, session: string) {
  available()
  const challenge = await api<{ challenge_id: string; options: { publicKey: Creation } }>(
    '/v1/admin/passkeys/register/start',
    { name },
    session
  )
  const options = challenge.options.publicKey
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
  return api(
    '/v1/admin/passkeys/register/finish',
    {
      challenge_id: challenge.challenge_id,
      credential: {
        id: credential.id,
        rawId: encode(credential.rawId),
        type: credential.type,
        response: {
          attestationObject: encode(response.attestationObject),
          clientDataJSON: encode(response.clientDataJSON),
          transports: response.getTransports?.() ?? [],
        },
        extensions: credential.getClientExtensionResults(),
      },
    },
    session
  )
}

export async function signInWithPasskey(): Promise<LoginResult> {
  available()
  const challenge = await api<{ challenge_id: string; options: { publicKey: Request } }>(
    '/v1/admin/passkeys/authenticate/start',
    {}
  )
  const options = challenge.options.publicKey
  const credential = (await navigator.credentials.get({
    publicKey: {
      ...options,
      challenge: decode(options.challenge),
      allowCredentials: options.allowCredentials?.map((item) => ({ ...item, id: decode(item.id) })),
    },
  })) as PublicKeyCredential | null
  if (!credential) throw new Error('未完成通行密钥验证。')
  const response = credential.response as AuthenticatorAssertionResponse
  return api<LoginResult>('/v1/admin/passkeys/authenticate/finish', {
    challenge_id: challenge.challenge_id,
    credential: {
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
    },
  })
}
