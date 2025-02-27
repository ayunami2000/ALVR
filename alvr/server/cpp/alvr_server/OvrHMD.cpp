#include "OvrHMD.h"

#include "ClientConnection.h"
#include "Logger.h"
#include "OvrController.h"
#include "OvrViveTrackerProxy.h"
#include "Paths.h"
#include "PoseHistory.h"
#include "Settings.h"
#include "Utils.h"
#include "bindings.h"
#include <cfloat>

#ifdef _WIN32
#include "platform/win32/CEncoder.h"
#elif __APPLE__
#include "platform/macos/CEncoder.h"
#else
#include "platform/linux/CEncoder.h"
#endif

const vr::HmdMatrix34_t MATRIX_IDENTITY = {
    {{1.0, 0.0, 0.0, 0.0}, {0.0, 1.0, 0.0, 0.0}, {0.0, 0.0, 1.0, 0.0}}};

vr::HmdRect2_t fov_to_projection(EyeFov fov) {
    auto proj_bounds = vr::HmdRect2_t{};
    proj_bounds.vTopLeft.v[0] = tanf(fov.left);
    proj_bounds.vBottomRight.v[0] = tanf(fov.right);
    proj_bounds.vTopLeft.v[1] = -tanf(fov.top);
    proj_bounds.vBottomRight.v[1] = -tanf(fov.bottom);

    return proj_bounds;
}

void fixInvalidHaptics(float hapticFeedback[3]) {
    // Assign a 5ms duration to legacy haptics pulses which otherwise have 0 duration and wouldn't
    // play.
    if (hapticFeedback[1] == 0.0f) {
        hapticFeedback[1] = 0.005f;
    }
}

inline vr::ETrackedDeviceClass getControllerDeviceClass() {
    // index == 8/9 == "HTCViveTracker.json"
    if (Settings::Instance().m_controllerMode == 8 || Settings::Instance().m_controllerMode == 9)
        return vr::TrackedDeviceClass_GenericTracker;
    return vr::TrackedDeviceClass_Controller;
}

OvrHmd::OvrHmd()
    : TrackedDevice(HEAD_ID), m_baseComponentsInitialized(false),
      m_streamComponentsInitialized(false) {
    auto dummy_fov = EyeFov{-1.0, 1.0, 1.0, -1.0};

    this->views_config = ViewsConfigData{};
    this->views_config.ipd_m = 0.063;
    this->views_config.fov[0] = dummy_fov;
    this->views_config.fov[1] = dummy_fov;

    m_pose = vr::DriverPose_t{};
    m_pose.poseIsValid = true;
    m_pose.result = vr::TrackingResult_Running_OK;
    m_pose.deviceIsConnected = true;
    m_pose.qWorldFromDriverRotation = HmdQuaternion_Init(1, 0, 0, 0);
    m_pose.qDriverFromHeadRotation = HmdQuaternion_Init(1, 0, 0, 0);
    m_pose.qRotation = HmdQuaternion_Init(1, 0, 0, 0);

    m_poseHistory = std::make_shared<PoseHistory>();

    m_deviceClass = Settings::Instance().m_TrackingRefOnly
                        ? vr::TrackedDeviceClass_TrackingReference
                        : vr::TrackedDeviceClass_HMD;
    bool ret;
    ret = vr::VRServerDriverHost()->TrackedDeviceAdded(
        GetSerialNumber().c_str(), m_deviceClass, this);
    if (!ret) {
        Warn("Failed to register device");
    }

    if (!Settings::Instance().m_disableController) {
        m_leftController = std::make_shared<OvrController>(LEFT_HAND_ID);
        ret = vr::VRServerDriverHost()->TrackedDeviceAdded(
            m_leftController->GetSerialNumber().c_str(),
            getControllerDeviceClass(),
            m_leftController.get());
        if (!ret) {
            Warn("Failed to register left controller");
        }

        m_rightController = std::make_shared<OvrController>(RIGHT_HAND_ID);
        ret = vr::VRServerDriverHost()->TrackedDeviceAdded(
            m_rightController->GetSerialNumber().c_str(),
            getControllerDeviceClass(),
            m_rightController.get());
        if (!ret) {
            Warn("Failed to register right controller");
        }
    }

    if (Settings::Instance().m_enableViveTrackerProxy) {
        m_viveTrackerProxy = std::make_shared<OvrViveTrackerProxy>(*this);
        ret = vr::VRServerDriverHost()->TrackedDeviceAdded(m_viveTrackerProxy->GetSerialNumber(),
                                                           vr::TrackedDeviceClass_GenericTracker,
                                                           m_viveTrackerProxy.get());
        if (!ret) {
            Warn("Failed to register Vive tracker");
        }
    }

    Debug("CRemoteHmd successfully initialized.\n");
}

OvrHmd::~OvrHmd() {
    ShutdownRuntime();

    if (m_encoder) {
        Debug("OvrHmd::~OvrHmd(): Stopping encoder...\n");
        m_encoder->Stop();
        m_encoder.reset();
    }

    if (m_Listener) {
        Debug("OvrHmd::~OvrHmd(): Stopping network...\n");
        m_Listener.reset();
    }

#ifdef _WIN32
    if (m_D3DRender) {
        m_D3DRender->Shutdown();
        m_D3DRender.reset();
    }
#endif
}

std::string OvrHmd::GetSerialNumber() const { return Settings::Instance().mSerialNumber; }

vr::EVRInitError OvrHmd::Activate(vr::TrackedDeviceIndex_t unObjectId) {
    Debug("CRemoteHmd Activate %d\n", unObjectId);

    auto vr_properties = vr::VRProperties();

    this->object_id = unObjectId;
    this->prop_container = vr_properties->TrackedDeviceToPropertyContainer(this->object_id);

    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_TrackingSystemName_String,
                                     Settings::Instance().mTrackingSystemName.c_str());
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_ModelNumber_String,
                                     Settings::Instance().mModelNumber.c_str());
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_ManufacturerName_String,
                                     Settings::Instance().mManufacturerName.c_str());
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_RenderModelName_String,
                                     Settings::Instance().mRenderModelName.c_str());
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_RegisteredDeviceType_String,
                                     Settings::Instance().mRegisteredDeviceType.c_str());
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_DriverVersion_String,
                                     Settings::Instance().mDriverVersion.c_str());
    vr_properties->SetFloatProperty(
        this->prop_container, vr::Prop_UserIpdMeters_Float, Settings::Instance().m_flIPD);
    vr_properties->SetFloatProperty(
        this->prop_container, vr::Prop_UserHeadToEyeDepthMeters_Float, 0.f);
    vr_properties->SetFloatProperty(this->prop_container,
                                    vr::Prop_DisplayFrequency_Float,
                                    static_cast<float>(Settings::Instance().m_refreshRate));
    vr_properties->SetFloatProperty(
        this->prop_container, vr::Prop_SecondsFromVsyncToPhotons_Float, 0.);
    // vr_properties->SetFloatProperty(this->prop_container,
    // vr::Prop_SecondsFromVsyncToPhotons_Float,
    // Settings::Instance().m_flSecondsFromVsyncToPhotons);

    // return a constant that's not 0 (invalid) or 1 (reserved for Oculus)
    vr_properties->SetUint64Property(
        this->prop_container, vr::Prop_CurrentUniverseId_Uint64, Settings::Instance().m_universeId);

#ifdef _WIN32
    // avoid "not fullscreen" warnings from vrmonitor
    vr_properties->SetBoolProperty(this->prop_container, vr::Prop_IsOnDesktop_Bool, false);

    // Manually send VSync events on direct mode.
    // ref:https://github.com/ValveSoftware/virtual_display/issues/1
    vr_properties->SetBoolProperty(
        this->prop_container, vr::Prop_DriverDirectModeSendsVsyncEvents_Bool, true);
#endif

    // Set battery as true
    vr_properties->SetBoolProperty(
        this->prop_container, vr::Prop_DeviceProvidesBatteryStatus_Bool, true);

#ifdef _WIN32
    float originalIPD =
        vr::VRSettings()->GetFloat(vr::k_pch_SteamVR_Section, vr::k_pch_SteamVR_IPD_Float);
    vr::VRSettings()->SetFloat(
        vr::k_pch_SteamVR_Section, vr::k_pch_SteamVR_IPD_Float, Settings::Instance().m_flIPD);
#endif

    HmdMatrix_SetIdentity(&m_eyeToHeadLeft);
    HmdMatrix_SetIdentity(&m_eyeToHeadRight);

    // set the icons in steamvr to the default icons used for Oculus Link
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_NamedIconPathDeviceOff_String,
                                     "{oculus}/icons/quest_headset_off.png");
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_NamedIconPathDeviceSearching_String,
                                     "{oculus}/icons/quest_headset_searching.gif");
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_NamedIconPathDeviceSearchingAlert_String,
                                     "{oculus}/icons/quest_headset_alert_searching.gif");
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_NamedIconPathDeviceReady_String,
                                     "{oculus}/icons/quest_headset_ready.png");
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_NamedIconPathDeviceReadyAlert_String,
                                     "{oculus}/icons/quest_headset_ready_alert.png");
    vr_properties->SetStringProperty(this->prop_container,
                                     vr::Prop_NamedIconPathDeviceStandby_String,
                                     "{oculus}/icons/quest_headset_standby.png");

// Disable async reprojection on Linux. Windows interface uses IVRDriverDirectModeComponent
// which never applies reprojection
// Also Disable async reprojection on vulkan
#ifndef _WIN32
    vr::VRSettings()->SetBool(
        vr::k_pch_SteamVR_Section, vr::k_pch_SteamVR_DisableAsyncReprojection_Bool, true);
    vr::VRSettings()->SetBool(vr::k_pch_SteamVR_Section,
                              vr::k_pch_SteamVR_EnableLinuxVulkanAsync_Bool,
                              Settings::Instance().m_enableLinuxVulkanAsync);
#endif

    if (!m_baseComponentsInitialized) {
        m_baseComponentsInitialized = true;

        if (IsHMD()) {
#ifdef _WIN32
            m_D3DRender = std::make_shared<CD3DRender>();

            // Use the same adapter as vrcompositor uses. If another adapter is used, vrcompositor
            // says "failed to open shared texture" and then crashes. It seems vrcompositor selects
            // always(?) first adapter. vrcompositor may use Intel iGPU when user sets it as primary
            // adapter. I don't know what happens on laptop which support optimus.
            // Prop_GraphicsAdapterLuid_Uint64 is only for redirect display and is ignored on direct
            // mode driver. So we can't specify an adapter for vrcompositor. m_nAdapterIndex is set
            // 0 on the launcher.
            if (!m_D3DRender->Initialize(Settings::Instance().m_nAdapterIndex)) {
                Error("Could not create graphics device for adapter %d.  Requires a minimum of two "
                      "graphics cards.\n",
                      Settings::Instance().m_nAdapterIndex);
                return vr::VRInitError_Driver_Failed;
            }

            int32_t nDisplayAdapterIndex;
            if (!m_D3DRender->GetAdapterInfo(&nDisplayAdapterIndex, m_adapterName)) {
                Error("Failed to get primary adapter info!\n");
                return vr::VRInitError_Driver_Failed;
            }

            Info("Using %ls as primary graphics adapter.\n", m_adapterName.c_str());
            Info("OSVer: %ls\n", GetWindowsOSVersion().c_str());

            m_directModeComponent =
                std::make_shared<OvrDirectModeComponent>(m_D3DRender, m_poseHistory);
#endif
        }

        DriverReadyIdle(IsHMD());
    }

    if (IsHMD()) {
        vr::VREvent_Data_t eventData;
        eventData.ipd = {Settings::Instance().m_flIPD};
        vr::VRServerDriverHost()->VendorSpecificEvent(
            this->object_id, vr::VREvent_IpdChanged, eventData, 0);
    }

    return vr::VRInitError_None;
}

void OvrHmd::Deactivate() {
    this->object_id = vr::k_unTrackedDeviceIndexInvalid;
    this->prop_container = vr::k_ulInvalidPropertyContainer;
}

void *OvrHmd::GetComponent(const char *component_name_and_version) {
    // NB: "this" pointer needs to be statically cast to point to the correct vtable

    auto name_and_vers = std::string(component_name_and_version);
    if (name_and_vers == vr::IVRDisplayComponent_Version) {
        return (vr::IVRDisplayComponent *)this;
    }

#ifdef _WIN32
    if (name_and_vers == vr::IVRDriverDirectModeComponent_Version) {
        return m_directModeComponent.get();
    }
#endif

    return nullptr;
}

vr::DriverPose_t OvrHmd::GetPose() { return m_pose; }

void OvrHmd::OnPoseUpdated(uint64_t targetTimestampNs, AlvrDeviceMotion motion) {
    if (this->object_id != vr::k_unTrackedDeviceIndexInvalid) {
        auto pose = vr::DriverPose_t{};
        pose.poseIsValid = true;
        pose.result = vr::TrackingResult_Running_OK;
        pose.deviceIsConnected = true;

        pose.qWorldFromDriverRotation = HmdQuaternion_Init(1, 0, 0, 0);
        pose.qDriverFromHeadRotation = HmdQuaternion_Init(1, 0, 0, 0);

        pose.qRotation = HmdQuaternion_Init(
            motion.orientation.w, motion.orientation.x, motion.orientation.y, motion.orientation.z);

        pose.vecPosition[0] = motion.position[0];
        pose.vecPosition[1] = motion.position[1];
        pose.vecPosition[2] = motion.position[2];

        m_pose = pose;

        m_poseHistory->OnPoseUpdated(targetTimestampNs, motion);

        vr::VRServerDriverHost()->TrackedDevicePoseUpdated(
            this->object_id, pose, sizeof(vr::DriverPose_t));

        if (m_viveTrackerProxy != nullptr)
            m_viveTrackerProxy->update();

#if !defined(_WIN32) && !defined(__APPLE__)
        // This has to be set after initialization is done, because something in vrcompositor is
        // setting it to 90Hz in the meantime
        if (!m_refreshRateSet && m_encoder && m_encoder->IsConnected()) {
            m_refreshRateSet = true;
            vr::VRProperties()->SetFloatProperty(
                this->prop_container,
                vr::Prop_DisplayFrequency_Float,
                static_cast<float>(Settings::Instance().m_refreshRate));
        }
#endif
    }
}

void OvrHmd::StartStreaming() {
    if (m_streamComponentsInitialized) {
        return;
    }

    // create listener
    m_Listener.reset(new ClientConnection());

    // Spin up a separate thread to handle the overlapped encoding/transmit step.
    if (IsHMD()) {
#ifdef _WIN32
        m_encoder = std::make_shared<CEncoder>();
        try {
            m_encoder->Initialize(m_D3DRender, m_Listener);
        } catch (Exception e) {
            Error("Your GPU does not meet the requirements for video encoding. %s %s\n%s %s\n",
                  "If you get this error after changing some settings, you can revert them by",
                  "deleting the file \"session.json\" in the installation folder.",
                  "Failed to initialize CEncoder:",
                  e.what());
        }
        m_encoder->Start();

        m_directModeComponent->SetEncoder(m_encoder);

        m_encoder->OnStreamStart();
#elif __APPLE__
        m_encoder = std::make_shared<CEncoder>();
#else
        m_encoder = std::make_shared<CEncoder>(m_Listener, m_poseHistory);
        m_encoder->Start();
#endif
    }

    m_streamComponentsInitialized = true;
}

void OvrHmd::SetViewsConfig(ViewsConfigData config) {
    this->views_config = config;

    auto left_transform = MATRIX_IDENTITY;
    left_transform.m[0][3] = -config.ipd_m / 2.0;
    auto right_transform = MATRIX_IDENTITY;
    right_transform.m[0][3] = config.ipd_m / 2.0;
    vr::VRServerDriverHost()->SetDisplayEyeToHead(object_id, left_transform, right_transform);

    auto left_proj = fov_to_projection(config.fov[0]);
    auto right_proj = fov_to_projection(config.fov[1]);

    vr::VRServerDriverHost()->SetDisplayProjectionRaw(object_id, left_proj, right_proj);

    // todo: check if this is still needed
    vr::VRServerDriverHost()->VendorSpecificEvent(
        object_id, vr::VREvent_LensDistortionChanged, {}, 0);
}

void OvrHmd::GetWindowBounds(int32_t *pnX, int32_t *pnY, uint32_t *pnWidth, uint32_t *pnHeight) {
    Debug("GetWindowBounds %dx%d - %dx%d\n",
          0,
          0,
          Settings::Instance().m_renderWidth,
          Settings::Instance().m_renderHeight);
    *pnX = 0;
    *pnY = 0;
    *pnWidth = Settings::Instance().m_renderWidth;
    *pnHeight = Settings::Instance().m_renderHeight;
}

bool OvrHmd::IsDisplayRealDisplay() {
#ifdef _WIN32
    return false;
#else
    return true;
#endif
}

void OvrHmd::GetRecommendedRenderTargetSize(uint32_t *pnWidth, uint32_t *pnHeight) {
    *pnWidth = Settings::Instance().m_recommendedTargetWidth / 2;
    *pnHeight = Settings::Instance().m_recommendedTargetHeight;
    Debug("GetRecommendedRenderTargetSize %dx%d\n", *pnWidth, *pnHeight);
}

void OvrHmd::GetEyeOutputViewport(
    vr::EVREye eEye, uint32_t *pnX, uint32_t *pnY, uint32_t *pnWidth, uint32_t *pnHeight) {
    *pnY = 0;
    *pnWidth = Settings::Instance().m_renderWidth / 2;
    *pnHeight = Settings::Instance().m_renderHeight;

    if (eEye == vr::Eye_Left) {
        *pnX = 0;
    } else {
        *pnX = Settings::Instance().m_renderWidth / 2;
    }
    Debug("GetEyeOutputViewport Eye=%d %dx%d %dx%d\n", eEye, *pnX, *pnY, *pnWidth, *pnHeight);
}

void OvrHmd::GetProjectionRaw(
    vr::EVREye eye, float *left, float *right, float *top, float *bottom) {
    auto proj = fov_to_projection(this->views_config.fov[eye]);
    *left = proj.vTopLeft.v[0];
    *right = proj.vBottomRight.v[0];
    *top = proj.vTopLeft.v[1];
    *bottom = proj.vBottomRight.v[1];
}

vr::DistortionCoordinates_t OvrHmd::ComputeDistortion(vr::EVREye, float u, float v) {
    return {{u, v}, {u, v}, {u, v}};
}
