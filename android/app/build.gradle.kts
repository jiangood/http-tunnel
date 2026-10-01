plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "me.rapiz.httptunnel"
    compileSdk = 34

    defaultConfig {
        applicationId = "me.rapiz.httptunnel"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.6.3"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    // The Rust library is built by `cargo build --target <android triple>`, and
    // copied into `jniLibs` manually. Android has no universal target, so each
    // ABI that should be shipped needs its own target and copy.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    packaging {
        resources.excludes += "/META-INF/{AL2.0,LGPL2.1}"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("com.google.android.material:material:1.12.0")
}
