use std::{
    ffi::{c_char, c_int, CStr, CString},
    fmt::Display,
    slice,
};

use crate::{KeyblockRef, KrbContext, Principal};

/// An error generated by libkadm5
#[derive(Debug)]
pub struct Error {
    pub code: krb5_sys::kadm5_ret_t,
}
impl Error {
    fn from_ret(code: krb5_sys::kadm5_ret_t) -> Result<(), Self> {
        if code.0 == krb5_sys::kadm5_ret_t(krb5_sys::KADM5_OK.into()).0 {
            Ok(())
        } else {
            Err(Self { code })
        }
    }
}
impl std::error::Error for Error {}
impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = unsafe { CStr::from_ptr(krb5_sys::error_message(self.code.0)) };
        f.write_str(&msg.to_string_lossy())
    }
}

/// Well-known error codes. This is not exhaustive.
pub mod error_code {
    pub use krb5_sys::kadm5_ret_t;
    pub const DUP: i64 = krb5_sys::KADM5_DUP as _;
}

/// Credentials that can be used to authenticate to kadm5.
pub enum Credential {
    /// A key stored in a keytab.
    ServiceKey {
        /// The path to the keytab containing the key.
        keytab: CString,
    },
}

#[derive(Default)]
/// Configuration fields for initializing libkadm5.
pub struct ConfigParams {
    /// The default realm.
    pub default_realm: Option<CString>,
    /// The hostname of the kadmin5 server.
    pub admin_server: Option<CString>,
    /// The port of the kadmin5 server.
    pub kadmind_port: Option<i32>,
}
impl ConfigParams {
    /// Return a [`krb5_sys::kadm5_config_params`] view of `self`
    ///
    /// The returned `kadm5_config_params` has the same lifetime as `&self`. It
    /// should be considered unusable as soon as `self` is moved, modified,
    /// or dropped.
    fn as_c(&self) -> krb5_sys::kadm5_config_params {
        let mut c = unsafe { std::mem::zeroed::<krb5_sys::kadm5_config_params>() };
        if let Some(default_realm) = &self.default_realm {
            c.realm = default_realm.as_ptr() as *mut c_char;
            c.mask |= i64::from(krb5_sys::KADM5_CONFIG_REALM);
        }
        if let Some(admin_server) = &self.admin_server {
            c.admin_server = admin_server.as_ptr() as *mut c_char;
            c.mask |= i64::from(krb5_sys::KADM5_CONFIG_ADMIN_SERVER);
        }
        if let Some(kadmind_port) = self.kadmind_port {
            c.kadmind_port = kadmind_port;
            c.mask |= i64::from(krb5_sys::KADM5_CONFIG_KADMIND_PORT);
        }
        c
    }
}

/// A kadmin5 client.
pub struct ServerHandle<'a> {
    ctx: &'a KrbContext,
    raw: *mut std::ffi::c_void,
}
impl<'a> ServerHandle<'a> {
    /// Create a new kadmin5 client.
    ///
    /// `client_name`: The principal of the kadmin5 client.
    /// `service_name`: The expected principal name of the kadmin5 server. Leave `None` to use the default principal.
    /// `credential`: The client credentials to be used.
    /// `params`: Any optional settings.
    pub fn new(
        ctx: &'a KrbContext,
        client_name: &CStr,
        service_name: Option<&CStr>,
        credential: &Credential,
        params: &ConfigParams,
    ) -> Result<Self, Error> {
        let mut server_handle = std::ptr::null_mut();
        let mut params = params.as_c();

        match credential {
            Credential::ServiceKey { keytab } => unsafe {
                Error::from_ret(krb5_sys::kadm5_init_with_skey(
                    ctx.raw,
                    client_name.as_ptr().cast_mut(),
                    keytab.as_ptr().cast_mut(),
                    service_name
                        .as_ref()
                        .map_or(std::ptr::null_mut(), |sn| sn.as_ptr().cast_mut()),
                    &mut params,
                    krb5_sys::KADM5_STRUCT_VERSION_1,
                    krb5_sys::KADM5_API_VERSION_4,
                    std::ptr::null_mut(),
                    &mut server_handle,
                ))?;
            },
        }
        Ok(Self {
            ctx,
            raw: server_handle,
        })
    }

    /// Create a new principal.
    pub fn create_principal(&self, principal: &Principal) -> Result<(), Error> {
        unsafe {
            let mut ent: krb5_sys::_kadm5_principal_ent_t = std::mem::zeroed();
            let mask = krb5_sys::KADM5_PRINCIPAL;
            ent.principal = principal.raw;
            Error::from_ret(krb5_sys::kadm5_create_principal(
                self.raw,
                &mut ent,
                mask.into(),
                std::ptr::null_mut(),
            ))
        }
    }

    /// Get the keys of a principal.
    ///
    /// `kvno` may specify a specific key version to retrieve. Set to [`KVNO_ALL`] to retrieve all keys.
    pub fn get_principal_keys(
        &self,
        principal: &Principal,
        kvno: krb5_sys::krb5_kvno,
    ) -> Result<KeyDataVec, Error> {
        let mut key_data = std::ptr::null_mut();
        let mut key_count = 0;
        unsafe {
            Error::from_ret(krb5_sys::kadm5_get_principal_keys(
                self.raw,
                principal.raw,
                kvno,
                &mut key_data,
                &mut key_count,
            ))?;
        }
        Ok(KeyDataVec {
            ctx: self.ctx,
            raw: key_data,
            key_count,
        })
    }
}
impl<'a> Drop for ServerHandle<'a> {
    fn drop(&mut self) {
        unsafe {
            Error::from_ret(krb5_sys::kadm5_destroy(self.raw))
                .expect("failed to destroy kadmin5 server handle");
        }
    }
}
/// Parameter for [`ServerHandle::get_principal_keys`] that returns all keys, regardless of KVNO.
pub const KVNO_ALL: krb5_sys::krb5_kvno = 0;

/// An unowned reference to a [`Principal`]'s key.
// SAFETY: 'a must not outlive the object that owns the `KeyDataRef`
pub struct KeyDataRef<'a> {
    pub kvno: krb5_sys::krb5_kvno,
    pub keyblock: KeyblockRef<'a>,
    // salt: krb5_sys::krb5_keysalt,
}
/// An owned reference to a set of keys associated with a [`Principal`].
pub struct KeyDataVec<'a> {
    ctx: &'a KrbContext,
    raw: *mut krb5_sys::kadm5_key_data,
    key_count: c_int,
}
impl KeyDataVec<'_> {
    // SAFETY: returned &kadm_key_data must not outlive &self
    fn as_slice(&self) -> &[krb5_sys::kadm5_key_data] {
        unsafe {
            slice::from_raw_parts(
                self.raw,
                self.key_count
                    .try_into()
                    .expect("keydata vec must have a non-negative number of keys"),
            )
        }
    }

    /// Iterate over all associated keys.
    #[allow(clippy::needless_lifetimes)]
    // SAFETY: returned KeyDataRef must not outlive &self
    pub fn keys<'a>(&'a self) -> impl Iterator<Item = KeyDataRef<'a>> {
        self.as_slice().iter().map(|raw| KeyDataRef {
            kvno: raw.kvno,
            keyblock: KeyblockRef {
                ctx: self.ctx,
                raw: &raw.key,
            },
            // salt: raw.salt,
        })
    }
}
impl Drop for KeyDataVec<'_> {
    fn drop(&mut self) {
        Error::from_ret(unsafe {
            krb5_sys::kadm5_free_kadm5_key_data(self.ctx.raw, self.key_count, self.raw)
        })
        .expect("failed to destroy keydata vector")
    }
}
